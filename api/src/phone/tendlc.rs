//! 10DLC texting registration (`tendlc_registrations`): US carriers block
//! business texts from local numbers until the company's Brand (the
//! business) and Campaign (how it texts) are registered with The Campaign
//! Registry through Telnyx.
//!
//! A company's Managers fill in the form ([`TenDlcDraft`]) from Admin →
//! Phone, save drafts and mark it ready; only a superuser sends it to Telnyx
//! (brands, campaigns and number assignments cost money), reads Telnyx's
//! answers and assigns the company's numbers to the approved campaign
//! ([`super::tendlc_registry`]).
//!
//! This module holds the form: its types, the checks that mirror Telnyx's
//! and the carriers' rules ([`problems`]), the suggested first draft, the
//! bodies sent to Telnyx and the stored rows. Nothing here logs the form's
//! contents.

use super::numbers::{is_nanp, to_e164};
use super::wire_enum;
use crate::telnyx::porting::mask_numbers;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, MySqlExecutor, MySqlPool};
use std::sync::LazyLock;
use utoipa::ToSchema;

/// Longest names, address parts, email and website (Telnyx's limit).
pub const NAME_CHARS: usize = 100;
/// The campaign description, in characters.
pub const DESCRIPTION_CHARS: (usize, usize) = (40, 4096);
/// The message flow (how customers opt in), in characters.
pub const MESSAGE_FLOW_CHARS: (usize, usize) = (40, 2048);
/// Each sample text, and the help, opt-in and opt-out answers.
pub const MESSAGE_CHARS: (usize, usize) = (20, 255);
/// Sample texts.
pub const SAMPLES: (usize, usize) = (2, 5);
/// Each keyword list, and each link.
pub const LIST_CHARS: usize = 255;
/// Sub use cases of `MIXED` and of `LOW_VOLUME`.
const MIXED_SUB_USECASES: (usize, usize) = (2, 5);
const LOW_VOLUME_SUB_USECASES: (usize, usize) = (1, 5);
/// Telnyx's brand status of a brand TCR refused.
pub const BRAND_FAILED: &str = "REGISTRATION_FAILED";
/// Telnyx's brand status of a registered brand.
pub const BRAND_OK: &str = "OK";

wire_enum! {
    /// Where a company's 10DLC registration is.
    pub enum TenDlcStatus {
        /// Being filled in by the company's Managers.
        Draft = "draft",
        /// A Manager marked it complete: a superuser reviews and sends it.
        Ready = "ready",
        /// Sent: Telnyx registers the brand (the business); the campaign follows.
        Sent = "sent",
        /// The brand is registered; Telnyx and the carriers review the campaign.
        CampaignPending = "campaign_pending",
        /// The carriers approved the campaign: the company's numbers can be assigned to it.
        Approved = "approved",
        /// Telnyx or a carrier refused it: fix what the feedback says, then it is sent again.
        Rejected = "rejected",
        /// The campaign was suspended or expired.
        Suspended = "suspended",
    }
}

impl TenDlcStatus {
    /// The form may still change (and be marked ready): nothing is with
    /// Telnyx, or Telnyx refused it.
    pub const fn editable(self) -> bool {
        matches!(self, Self::Draft | Self::Ready | Self::Rejected)
    }
}

wire_enum! {
    /// The form of business behind the brand.
    pub enum EntityType {
        /// A private company: LLC, Inc., partnership.
        PrivateProfit = "PRIVATE_PROFIT",
        /// A publicly traded company (stock symbol and exchange needed).
        PublicProfit = "PUBLIC_PROFIT",
        /// A registered non-profit (501(c)(3) or similar).
        NonProfit = "NON_PROFIT",
        /// A government body.
        Government = "GOVERNMENT",
    }
}

wire_enum! {
    /// The brand's industry (TCR's verticals).
    pub enum Vertical {
        Agriculture = "AGRICULTURE",
        Communication = "COMMUNICATION",
        /// Construction and remodeling: countertop fabricators.
        Construction = "CONSTRUCTION",
        Education = "EDUCATION",
        Energy = "ENERGY",
        Entertainment = "ENTERTAINMENT",
        Financial = "FINANCIAL",
        Gambling = "GAMBLING",
        Government = "GOVERNMENT",
        Healthcare = "HEALTHCARE",
        Hospitality = "HOSPITALITY",
        HumanResources = "HUMAN_RESOURCES",
        Insurance = "INSURANCE",
        Legal = "LEGAL",
        Manufacturing = "MANUFACTURING",
        Ngo = "NGO",
        Political = "POLITICAL",
        Postal = "POSTAL",
        Professional = "PROFESSIONAL",
        RealEstate = "REAL_ESTATE",
        Retail = "RETAIL",
        Technology = "TECHNOLOGY",
        Transportation = "TRANSPORTATION",
    }
}

wire_enum! {
    /// A public company's stock exchange.
    pub enum StockExchange {
        Nasdaq = "NASDAQ",
        Nyse = "NYSE",
        Amex = "AMEX",
        Amx = "AMX",
        Asx = "ASX",
        B3 = "B3",
        Bme = "BME",
        Bse = "BSE",
        Fra = "FRA",
        Icex = "ICEX",
        Jpx = "JPX",
        Jse = "JSE",
        Krx = "KRX",
        Lon = "LON",
        Nse = "NSE",
        Omx = "OMX",
        Sehk = "SEHK",
        Sse = "SSE",
        Sto = "STO",
        Swx = "SWX",
        Szse = "SZSE",
        Tsx = "TSX",
        Twse = "TWSE",
        Vse = "VSE",
    }
}

wire_enum! {
    /// What the texts are about (TCR's campaign use case).
    pub enum UseCase {
        /// Several kinds of texts (pick 2 to 5 sub use cases): the usual choice.
        Mixed = "MIXED",
        /// Under 6,000 texts a month of several kinds (pick 1 to 5 sub use cases).
        LowVolume = "LOW_VOLUME",
        /// Answers and updates for customers who contacted the business.
        CustomerCare = "CUSTOMER_CARE",
        /// Account updates: quotes, invoices, appointments.
        AccountNotification = "ACCOUNT_NOTIFICATION",
        /// Order and delivery updates.
        DeliveryNotification = "DELIVERY_NOTIFICATION",
        /// Promotions and offers.
        Marketing = "MARKETING",
        /// Surveys and polls.
        PollingVoting = "POLLING_VOTING",
        /// One-time sign-in codes.
        TwoFactor = "2FA",
        /// Security alerts.
        SecurityAlert = "SECURITY_ALERT",
        /// Fraud alerts.
        FraudAlert = "FRAUD_ALERT",
        /// Public service announcements.
        PublicServiceAnnouncement = "PUBLIC_SERVICE_ANNOUNCEMENT",
        /// Colleges and universities.
        HigherEducation = "HIGHER_EDUCATION",
    }
}

impl UseCase {
    /// How many sub use cases it needs (none for most).
    pub const fn sub_usecases(self) -> Option<(usize, usize)> {
        match self {
            Self::Mixed => Some(MIXED_SUB_USECASES),
            Self::LowVolume => Some(LOW_VOLUME_SUB_USECASES),
            _ => None,
        }
    }
}

/// The business behind the texts (the 10DLC brand). Any field may be empty
/// while a draft; `problems` says what sending needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct TenDlcBrandDraft {
    /// The form of business; null until chosen.
    pub entity_type: Option<EntityType>,
    /// The name customers know the business by (DBA), shown to carriers; up
    /// to 100 characters.
    pub display_name: String,
    /// The legal name, exactly as the IRS (or CRA) has it for the tax id;
    /// up to 100 characters.
    pub company_name: String,
    /// The tax id: a US EIN (`12-3456789`) or a Canadian business number
    /// (9 digits).
    pub ein: String,
    /// Where the business and its tax id are registered: `US` or `CA`.
    pub country: String,
    /// A business contact's first name; may be empty. Up to 100 characters.
    pub first_name: String,
    /// A business contact's last name; may be empty. Up to 100 characters.
    pub last_name: String,
    /// The business phone, a US or Canada number (saved as E.164 when it is
    /// one).
    pub phone: String,
    /// The business email carriers may write to; up to 100 characters.
    pub email: String,
    /// The business website (`https://...`), which carriers check; up to
    /// 100 characters.
    pub website: String,
    /// Street and number of the business address; up to 100 characters.
    pub street: String,
    /// City; up to 100 characters.
    pub city: String,
    /// State or province, 2 letters (`IN`).
    pub state: String,
    /// ZIP code (`46204`) or Canadian postal code (`K1A 0B1`).
    pub postal_code: String,
    /// The industry; null until chosen.
    pub vertical: Option<Vertical>,
    /// `PUBLIC_PROFIT` only: the stock symbol, up to 10 characters.
    pub stock_symbol: String,
    /// `PUBLIC_PROFIT` only: the stock exchange; null otherwise.
    pub stock_exchange: Option<StockExchange>,
    /// `PUBLIC_PROFIT` only: the business contact email Telnyx requires.
    pub business_contact_email: String,
}

/// How the business texts (the 10DLC campaign). Any field may be empty
/// while a draft; `problems` says what sending needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct TenDlcCampaignDraft {
    /// What the texts are about; null until chosen.
    pub usecase: Option<UseCase>,
    /// The kinds of texts of a `MIXED` (2 to 5) or `LOW_VOLUME` (1 to 5)
    /// campaign; empty for the other use cases.
    pub sub_usecases: Vec<UseCase>,
    /// What the business texts and why, 40 to 4096 characters.
    pub description: String,
    /// How customers agree to get texts: where they give their number and
    /// what they are told; 40 to 2048 characters.
    pub message_flow: String,
    /// Example texts, 2 to 5, each 20 to 255 characters, naming the
    /// business and saying how to opt out ("Reply STOP to opt out").
    pub samples: Vec<String>,
    /// The answer to HELP: names the business and how to reach it; 20 to
    /// 255 characters.
    pub help_message: String,
    /// The words that get the help answer, comma separated (`HELP,INFO`);
    /// must include HELP.
    pub help_keywords: String,
    /// The answer to an opt-in word, naming the business; 20 to 255
    /// characters.
    pub optin_message: String,
    /// Opt-in words, comma separated (`START`).
    pub optin_keywords: String,
    /// The answer to STOP, naming the business; 20 to 255 characters.
    pub optout_message: String,
    /// Opt-out words, comma separated; must include STOP.
    pub optout_keywords: String,
    /// The texts carry links (needed when a sample has one).
    pub embedded_link: bool,
    /// An example of a link the texts carry; may be empty. Up to 255
    /// characters.
    pub embedded_link_sample: String,
    /// The texts carry phone numbers (needed when a sample has one).
    pub embedded_phone: bool,
    /// The texts go out from a pool of numbers (50 or more).
    pub number_pool: bool,
    /// The texts are about age-restricted products.
    pub age_gated: bool,
    /// The texts are about direct lending or loans.
    pub direct_lending: bool,
    /// The privacy policy page (`https://...`); may be empty.
    pub privacy_policy_link: String,
    /// The texting terms page (`https://...`); may be empty.
    pub terms_and_conditions_link: String,
    /// The carriers' terms for business texting are accepted (needed to
    /// send).
    pub terms_accepted: bool,
}

/// The registration form: the business and how it texts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct TenDlcDraft {
    /// The business (Brand).
    pub brand: TenDlcBrandDraft,
    /// How it texts (Campaign).
    pub campaign: TenDlcCampaignDraft,
}

/// What blocks marking the form ready or sending it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TenDlcProblem {
    /// The field, as a path of the form: `brand.ein`, `campaign.samples.1`,
    /// `campaign.subUsecases`.
    pub field: String,
    /// What is wrong, for the person filling it in.
    pub message: String,
}

fn problem(field: &str, message: impl Into<String>) -> TenDlcProblem {
    TenDlcProblem {
        field: field.to_string(),
        message: message.into(),
    }
}

/// One of Telnyx's (or a carrier's) reasons, phone numbers masked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TenDlcFeedback {
    /// What it is about, e.g. "Tax Id", "Carrier review".
    pub title: String,
    /// What is wrong, in Telnyx's words.
    pub description: String,
    /// The form fields it names (Telnyx's names, e.g. `ein`, `sample1`);
    /// may be empty.
    #[serde(default)]
    pub fields: Vec<String>,
}

// ---------------------------------------------------------------- checks

static ZIP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{5}(-\d{4})?$").expect("valid pattern"));
static CANADIAN_POSTAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z]\d[A-Z] ?\d[A-Z]\d$").expect("valid pattern"));
static US_EIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{2}-?\d{7}$").expect("valid pattern"));
static CANADIAN_BN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{9}([A-Z]{2}\d{4})?$").expect("valid pattern"));
static EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[^@\s]+@[^@\s]+\.[^@\s]+$").expect("valid pattern"));
static STOCK_SYMBOL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z0-9.\-]{1,10}$").expect("valid pattern"));
static KEYWORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z0-9]{1,30}$").expect("valid pattern"));
static LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(https?://|www\.)\S+|\b[a-z0-9-]+\.(com|net|org|us|biz|info|co|io)\b")
        .expect("valid pattern")
});
static SHORTENER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(bit\.ly|tinyurl\.com|goo\.gl|t\.co|ow\.ly|is\.gd|buff\.ly|rebrand\.ly|cutt\.ly|shorturl\.at|tiny\.cc)\b",
    )
    .expect("valid pattern")
});
static OPT_OUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bstop\b").expect("valid pattern"));

fn chars(text: &str) -> usize {
    text.chars().count()
}

/// An `http(s)://` URL with a host.
fn web_url(text: &str) -> bool {
    (text.starts_with("https://") || text.starts_with("http://"))
        && url::Url::parse(text)
            .is_ok_and(|url| url.host_str().is_some_and(|host| host.contains('.')))
}

/// The tax id as Telnyx takes it: `12-3456789` for a US EIN, the 9 digits
/// (and program account) of a Canadian business number; `None` when it is
/// neither.
pub fn tax_id(ein: &str, country: &str) -> Option<String> {
    let compact: String = ein
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_uppercase();
    if country == "CA" {
        let compact = compact.replace('-', "");
        return CANADIAN_BN.is_match(&compact).then_some(compact);
    }
    if !US_EIN.is_match(&compact) {
        return None;
    }
    let digits: String = compact.chars().filter(char::is_ascii_digit).collect();
    Some(format!("{}-{}", &digits[..2], &digits[2..]))
}

/// The keywords of a list, upper case, once each.
pub fn keywords(list: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in list.split(|c: char| c == ',' || c.is_whitespace()) {
        let word = word.trim().to_uppercase();
        if !word.is_empty() && !words.contains(&word) {
            words.push(word);
        }
    }
    words
}

/// The names a text must mention (the display and legal names), lower case.
fn business_names(brand: &TenDlcBrandDraft) -> Vec<String> {
    [&brand.display_name, &brand.company_name]
        .into_iter()
        .map(|name| name.trim().to_lowercase())
        .filter(|name| !name.is_empty())
        .collect()
}

fn mentions(text: &str, names: &[String]) -> bool {
    let text = text.to_lowercase();
    names.iter().any(|name| text.contains(name.as_str()))
}

/// The form as saved: text trimmed, the phone as E.164 when it is a US or
/// Canada number, codes upper case, keyword lists `A,B`, empty samples
/// dropped, sub use cases only for the use cases that have them.
pub fn normalized(draft: &TenDlcDraft) -> TenDlcDraft {
    let brand = &draft.brand;
    let campaign = &draft.campaign;
    let trim = |text: &String| text.trim().to_string();
    let phone = brand.phone.trim();
    let usecase = campaign.usecase;
    let mut sub_usecases: Vec<UseCase> = Vec::new();
    if usecase.and_then(UseCase::sub_usecases).is_some() {
        for sub in &campaign.sub_usecases {
            if !sub_usecases.contains(sub) {
                sub_usecases.push(*sub);
            }
        }
    }
    TenDlcDraft {
        brand: TenDlcBrandDraft {
            entity_type: brand.entity_type,
            display_name: trim(&brand.display_name),
            company_name: trim(&brand.company_name),
            ein: trim(&brand.ein),
            country: brand.country.trim().to_uppercase(),
            first_name: trim(&brand.first_name),
            last_name: trim(&brand.last_name),
            phone: to_e164(phone)
                .filter(|e164| is_nanp(e164))
                .unwrap_or_else(|| phone.to_string()),
            email: trim(&brand.email),
            website: trim(&brand.website),
            street: trim(&brand.street),
            city: trim(&brand.city),
            state: brand.state.trim().to_uppercase(),
            postal_code: brand.postal_code.trim().to_uppercase(),
            vertical: brand.vertical,
            stock_symbol: brand.stock_symbol.trim().to_uppercase(),
            stock_exchange: brand.stock_exchange,
            business_contact_email: trim(&brand.business_contact_email),
        },
        campaign: TenDlcCampaignDraft {
            usecase,
            sub_usecases,
            description: trim(&campaign.description),
            message_flow: trim(&campaign.message_flow),
            samples: campaign
                .samples
                .iter()
                .map(|sample| sample.trim().to_string())
                .filter(|sample| !sample.is_empty())
                .collect(),
            help_message: trim(&campaign.help_message),
            help_keywords: keywords(&campaign.help_keywords).join(","),
            optin_message: trim(&campaign.optin_message),
            optin_keywords: keywords(&campaign.optin_keywords).join(","),
            optout_message: trim(&campaign.optout_message),
            optout_keywords: keywords(&campaign.optout_keywords).join(","),
            embedded_link: campaign.embedded_link,
            embedded_link_sample: trim(&campaign.embedded_link_sample),
            embedded_phone: campaign.embedded_phone,
            number_pool: campaign.number_pool,
            age_gated: campaign.age_gated,
            direct_lending: campaign.direct_lending,
            privacy_policy_link: trim(&campaign.privacy_policy_link),
            terms_and_conditions_link: trim(&campaign.terms_and_conditions_link),
            terms_accepted: campaign.terms_accepted,
        },
    }
}

/// The longest each field may be (Telnyx's limits): a message naming the
/// first field over its limit, for a save to refuse. Empty fields are fine.
pub fn too_long(draft: &TenDlcDraft) -> Option<String> {
    let brand = &draft.brand;
    let campaign = &draft.campaign;
    let limits: [(&str, &str, usize); 25] = [
        ("business name", &brand.display_name, NAME_CHARS),
        ("legal name", &brand.company_name, NAME_CHARS),
        ("tax id", &brand.ein, 20),
        ("country", &brand.country, 2),
        ("first name", &brand.first_name, NAME_CHARS),
        ("last name", &brand.last_name, NAME_CHARS),
        ("phone", &brand.phone, 20),
        ("email", &brand.email, NAME_CHARS),
        ("website", &brand.website, NAME_CHARS),
        ("street address", &brand.street, NAME_CHARS),
        ("city", &brand.city, NAME_CHARS),
        ("state", &brand.state, 20),
        ("ZIP or postal code", &brand.postal_code, 10),
        ("stock symbol", &brand.stock_symbol, 10),
        (
            "business contact email",
            &brand.business_contact_email,
            NAME_CHARS,
        ),
        ("description", &campaign.description, DESCRIPTION_CHARS.1),
        ("message flow", &campaign.message_flow, MESSAGE_FLOW_CHARS.1),
        ("help message", &campaign.help_message, MESSAGE_CHARS.1),
        ("help keywords", &campaign.help_keywords, LIST_CHARS),
        ("opt-in message", &campaign.optin_message, MESSAGE_CHARS.1),
        ("opt-in keywords", &campaign.optin_keywords, LIST_CHARS),
        ("opt-out message", &campaign.optout_message, MESSAGE_CHARS.1),
        ("opt-out keywords", &campaign.optout_keywords, LIST_CHARS),
        ("example link", &campaign.embedded_link_sample, LIST_CHARS),
        (
            "privacy policy link",
            &campaign.privacy_policy_link,
            LIST_CHARS,
        ),
    ];
    for (name, value, max) in limits {
        if chars(value) > max {
            return Some(format!("Use at most {max} characters for the {name}."));
        }
    }
    if chars(&campaign.terms_and_conditions_link) > LIST_CHARS {
        return Some(format!(
            "Use at most {LIST_CHARS} characters for the terms link."
        ));
    }
    if campaign.samples.len() > SAMPLES.1 {
        return Some(format!("Write at most {} sample texts.", SAMPLES.1));
    }
    if let Some(at) = campaign
        .samples
        .iter()
        .position(|sample| chars(sample) > MESSAGE_CHARS.1)
    {
        return Some(format!(
            "Sample text {} is too long: at most {} characters.",
            at + 1,
            MESSAGE_CHARS.1
        ));
    }
    if campaign.sub_usecases.len() > 5 {
        return Some("Pick at most 5 kinds of texts.".into());
    }
    None
}

/// Checks a text that must be there: `min` to `max` characters.
fn length(
    problems: &mut Vec<TenDlcProblem>,
    field: &str,
    name: &str,
    text: &str,
    (min, max): (usize, usize),
) -> bool {
    let count = chars(text);
    if count == 0 {
        problems.push(problem(field, format!("Enter the {name}.")));
        false
    } else if count < min {
        problems.push(problem(
            field,
            format!("Write at least {min} characters for the {name}."),
        ));
        false
    } else if count > max {
        problems.push(problem(
            field,
            format!("Use at most {max} characters for the {name}."),
        ));
        false
    } else {
        true
    }
}

fn brand_problems(brand: &TenDlcBrandDraft, problems: &mut Vec<TenDlcProblem>) {
    if brand.entity_type.is_none() {
        problems.push(problem("brand.entityType", "Choose the type of business."));
    }
    length(
        problems,
        "brand.displayName",
        "business name",
        &brand.display_name,
        (1, NAME_CHARS),
    );
    length(
        problems,
        "brand.companyName",
        "legal name",
        &brand.company_name,
        (1, NAME_CHARS),
    );
    let country = brand.country.as_str();
    if country != "US" && country != "CA" {
        problems.push(problem(
            "brand.country",
            "The business must be registered in the US or Canada.",
        ));
    }
    if brand.ein.is_empty() {
        problems.push(problem("brand.ein", "Enter the tax id (EIN)."));
    } else if tax_id(&brand.ein, country).is_none() {
        problems.push(problem(
            "brand.ein",
            if country == "CA" {
                "Enter the 9-digit business number."
            } else {
                "Enter the EIN as 9 digits, like 12-3456789."
            },
        ));
    }
    if !to_e164(&brand.phone).is_some_and(|e164| is_nanp(&e164)) {
        problems.push(problem(
            "brand.phone",
            "Enter the business phone, a US or Canada number.",
        ));
    }
    if !(EMAIL.is_match(&brand.email) && chars(&brand.email) <= NAME_CHARS) {
        problems.push(problem("brand.email", "Enter the business email."));
    }
    if !(web_url(&brand.website) && chars(&brand.website) <= NAME_CHARS) {
        problems.push(problem(
            "brand.website",
            "Enter the business website, starting with https://.",
        ));
    }
    length(
        problems,
        "brand.street",
        "street address",
        &brand.street,
        (1, NAME_CHARS),
    );
    length(problems, "brand.city", "city", &brand.city, (1, NAME_CHARS));
    if !(brand.state.len() == 2 && brand.state.chars().all(|c| c.is_ascii_uppercase())) {
        problems.push(problem(
            "brand.state",
            "Enter the state or province as 2 letters.",
        ));
    }
    let postal_ok = if country == "CA" {
        CANADIAN_POSTAL.is_match(&brand.postal_code)
    } else {
        ZIP.is_match(&brand.postal_code)
    };
    if !postal_ok {
        problems.push(problem(
            "brand.postalCode",
            "Enter a valid ZIP or postal code.",
        ));
    }
    if brand.vertical.is_none() {
        problems.push(problem("brand.vertical", "Choose the industry."));
    }
    if brand.entity_type == Some(EntityType::PublicProfit) {
        if !STOCK_SYMBOL.is_match(&brand.stock_symbol) {
            problems.push(problem(
                "brand.stockSymbol",
                "Enter the stock symbol of the public company.",
            ));
        }
        if brand.stock_exchange.is_none() {
            problems.push(problem("brand.stockExchange", "Choose the stock exchange."));
        }
        if !EMAIL.is_match(&brand.business_contact_email) {
            problems.push(problem(
                "brand.businessContactEmail",
                "Enter the business contact email (needed for public companies).",
            ));
        }
    }
}

fn keyword_problems(
    problems: &mut Vec<TenDlcProblem>,
    field: &str,
    name: &str,
    list: &str,
    required: Option<&str>,
) {
    let words = keywords(list);
    if words.is_empty() {
        problems.push(problem(field, format!("Enter the {name}.")));
    } else if let Some(word) = words.iter().find(|word| !KEYWORD.is_match(word)) {
        problems.push(problem(
            field,
            format!(
                "Keywords are single words of letters and digits ({} is not).",
                word.chars().take(30).collect::<String>()
            ),
        ));
    } else if let Some(required) = required
        && !words.iter().any(|word| word == required)
    {
        problems.push(problem(
            field,
            format!("The {name} must include {required}."),
        ));
    }
}

fn campaign_problems(
    campaign: &TenDlcCampaignDraft,
    names: &[String],
    problems: &mut Vec<TenDlcProblem>,
) {
    match campaign.usecase {
        None => problems.push(problem(
            "campaign.usecase",
            "Choose what the texts are about.",
        )),
        Some(usecase) => {
            if let Some((min, max)) = usecase.sub_usecases() {
                let count = campaign.sub_usecases.len();
                if count < min || count > max {
                    problems.push(problem(
                        "campaign.subUsecases",
                        format!("Pick {min} to {max} kinds of texts."),
                    ));
                } else if campaign
                    .sub_usecases
                    .iter()
                    .any(|sub| sub.sub_usecases().is_some())
                {
                    problems.push(problem(
                        "campaign.subUsecases",
                        "Mixed and low volume are not kinds of texts.",
                    ));
                }
            }
        }
    }
    length(
        problems,
        "campaign.description",
        "description",
        &campaign.description,
        DESCRIPTION_CHARS,
    );
    length(
        problems,
        "campaign.messageFlow",
        "message flow (how customers agree to texts)",
        &campaign.message_flow,
        MESSAGE_FLOW_CHARS,
    );
    let samples = &campaign.samples;
    if samples.len() < SAMPLES.0 {
        problems.push(problem(
            "campaign.samples",
            format!("Write at least {} sample texts.", SAMPLES.0),
        ));
    } else if samples.len() > SAMPLES.1 {
        problems.push(problem(
            "campaign.samples",
            format!("Write at most {} sample texts.", SAMPLES.1),
        ));
    }
    let mut has_link = false;
    let mut has_phone = false;
    for (at, sample) in samples.iter().enumerate() {
        let field = format!("campaign.samples.{at}");
        let name = format!("sample text {}", at + 1);
        if !length(problems, &field, &name, sample, MESSAGE_CHARS) {
            continue;
        }
        if !names.is_empty() && !mentions(sample, names) {
            problems.push(problem(
                &field,
                format!("Sample text {} must name the business.", at + 1),
            ));
        } else if !OPT_OUT.is_match(sample) {
            problems.push(problem(
                &field,
                format!(
                    "Sample text {} must say how to opt out, like \"Reply STOP to opt out\".",
                    at + 1
                ),
            ));
        } else if SHORTENER.is_match(sample) {
            problems.push(problem(
                &field,
                format!(
                    "Sample text {} uses a link shortener: carriers refuse them. Use the full link.",
                    at + 1
                ),
            ));
        } else if samples[..at]
            .iter()
            .any(|earlier| earlier.eq_ignore_ascii_case(sample))
        {
            problems.push(problem(
                &field,
                format!(
                    "Sample text {} repeats another: make each different.",
                    at + 1
                ),
            ));
        }
        has_link |= LINK.is_match(sample);
        has_phone |= mask_numbers(sample) != *sample;
    }
    if has_link && !campaign.embedded_link {
        problems.push(problem(
            "campaign.embeddedLink",
            "A sample text has a link: say the texts carry links.",
        ));
    }
    if has_phone && !campaign.embedded_phone {
        problems.push(problem(
            "campaign.embeddedPhone",
            "A sample text has a phone number: say the texts carry phone numbers.",
        ));
    }
    for (field, name, text) in [
        (
            "campaign.helpMessage",
            "help message",
            &campaign.help_message,
        ),
        (
            "campaign.optinMessage",
            "opt-in message",
            &campaign.optin_message,
        ),
        (
            "campaign.optoutMessage",
            "opt-out message",
            &campaign.optout_message,
        ),
    ] {
        if length(problems, field, name, text, MESSAGE_CHARS)
            && !names.is_empty()
            && !mentions(text, names)
        {
            problems.push(problem(
                field,
                format!("The {name} must name the business."),
            ));
        }
    }
    keyword_problems(
        problems,
        "campaign.helpKeywords",
        "help keywords",
        &campaign.help_keywords,
        Some("HELP"),
    );
    keyword_problems(
        problems,
        "campaign.optinKeywords",
        "opt-in keywords",
        &campaign.optin_keywords,
        None,
    );
    keyword_problems(
        problems,
        "campaign.optoutKeywords",
        "opt-out keywords",
        &campaign.optout_keywords,
        Some("STOP"),
    );
    for (field, name, link) in [
        (
            "campaign.embeddedLinkSample",
            "example link",
            &campaign.embedded_link_sample,
        ),
        (
            "campaign.privacyPolicyLink",
            "privacy policy link",
            &campaign.privacy_policy_link,
        ),
        (
            "campaign.termsAndConditionsLink",
            "terms link",
            &campaign.terms_and_conditions_link,
        ),
    ] {
        if !link.is_empty() && !web_url(link) {
            problems.push(problem(
                field,
                format!("Enter the {name} as a full address starting with https://."),
            ));
        }
    }
    if !campaign.terms_accepted {
        problems.push(problem(
            "campaign.termsAccepted",
            "Accept the carriers' terms for business texting.",
        ));
    }
}

/// What blocks marking the (normalized) form ready or sending it: every
/// field Telnyx and the carriers need, in the formats they take, and the
/// carriers' content rules (each sample names the business and says how to
/// opt out; links and phone numbers declared; no link shorteners). Empty
/// when it is complete.
pub fn problems(draft: &TenDlcDraft) -> Vec<TenDlcProblem> {
    let mut problems = Vec::new();
    brand_problems(&draft.brand, &mut problems);
    campaign_problems(
        &draft.campaign,
        &business_names(&draft.brand),
        &mut problems,
    );
    problems
}

/// The campaign fields Telnyx no longer takes once a campaign is registered
/// (all but the samples, message flow and help message) differ.
pub fn locked_fields_changed(sent: &TenDlcCampaignDraft, now: &TenDlcCampaignDraft) -> bool {
    let editable = |campaign: &TenDlcCampaignDraft| TenDlcCampaignDraft {
        samples: Vec::new(),
        message_flow: String::new(),
        help_message: String::new(),
        ..campaign.clone()
    };
    editable(sent) != editable(now)
}

/// The fields Telnyx takes on a registered campaign (samples, message flow,
/// help message) differ.
pub fn editable_fields_changed(sent: &TenDlcCampaignDraft, now: &TenDlcCampaignDraft) -> bool {
    sent.samples != now.samples
        || sent.message_flow != now.message_flow
        || sent.help_message != now.help_message
}

// ------------------------------------------------------------- suggestion

/// A first draft for a company that has none: its name in every text,
/// "Reply STOP to opt out" samples, a mixed campaign of customer care,
/// account updates and offers, and what the CRM knows (the Manager's email
/// and name, the company's main line and its domain as the website).
pub fn suggestion(
    company_name: &str,
    domain: Option<&str>,
    main_line: Option<&str>,
    email: &str,
    contact_name: Option<&str>,
) -> TenDlcDraft {
    let name = company_name.trim();
    let mut contact = contact_name.unwrap_or_default().split_whitespace();
    let first_name = contact.next().unwrap_or_default().to_string();
    let last_name = contact.collect::<Vec<_>>().join(" ");
    let call = main_line
        .map(|line| format!(" or call {}", display_number(line)))
        .unwrap_or_default();
    TenDlcDraft {
        brand: TenDlcBrandDraft {
            entity_type: Some(EntityType::PrivateProfit),
            display_name: name.chars().take(NAME_CHARS).collect(),
            company_name: name.chars().take(NAME_CHARS).collect(),
            country: "US".into(),
            first_name: first_name.chars().take(NAME_CHARS).collect(),
            last_name: last_name.chars().take(NAME_CHARS).collect(),
            phone: main_line.unwrap_or_default().to_string(),
            email: email.trim().chars().take(NAME_CHARS).collect(),
            website: domain
                .map(str::trim)
                .filter(|domain| !domain.is_empty() && !domain.contains('/'))
                .map(|domain| format!("https://{domain}"))
                .unwrap_or_default(),
            vertical: Some(Vertical::Construction),
            ..TenDlcBrandDraft::default()
        },
        campaign: TenDlcCampaignDraft {
            usecase: Some(UseCase::Mixed),
            sub_usecases: vec![
                UseCase::CustomerCare,
                UseCase::AccountNotification,
                UseCase::Marketing,
            ],
            description: format!(
                "{name} texts customers who asked for a quote or hired us: answers to their \
                 questions, quotes, appointment confirmations and reminders for measuring and \
                 installation, project updates, and occasional offers to customers who agreed \
                 to receive them."
            ),
            message_flow: format!(
                "Customers give {name} their mobile number when they request a quote on our \
                 website, call us or visit our showroom, and agree to receive texts. They are \
                 told: \"By giving us your number you agree to receive texts from {name} about \
                 your project, appointments and offers. Msg frequency varies. Msg & data rates \
                 may apply. Reply HELP for help, STOP to opt out.\" We only text customers who \
                 agreed and stop as soon as they reply STOP."
            ),
            samples: vec![
                format!(
                    "{name}: Hi Sarah, your countertop quote is ready. Reply with any \
                     questions. Reply STOP to opt out."
                ),
                format!(
                    "{name}: Reminder: your measuring appointment is tomorrow between 9 and \
                     11 AM. Reply C to confirm. Reply STOP to opt out."
                ),
                format!(
                    "{name}: Thanks for choosing us! This month past customers get 10% off \
                     vanity tops. Reply STOP to opt out."
                ),
            ],
            help_message: format!(
                "{name}: for help reply to this text{call}. Msg & data rates may apply. Reply \
                 STOP to opt out."
            ),
            help_keywords: "HELP,INFO".into(),
            optin_message: format!(
                "{name}: you will get texts about your project, appointments and offers. Msg \
                 frequency varies. Msg & data rates may apply. Reply HELP for help, STOP to opt \
                 out."
            ),
            optin_keywords: "START,UNSTOP".into(),
            optout_message: format!(
                "{name}: you are unsubscribed and will get no more texts. Reply START to \
                 subscribe again."
            ),
            optout_keywords: "STOP,STOPALL,UNSUBSCRIBE,CANCEL,END,QUIT".into(),
            ..TenDlcCampaignDraft::default()
        },
    }
}

/// `+13175550100` as `317-555-0100` (US and Canada), else as it is.
fn display_number(e164: &str) -> String {
    match e164.strip_prefix("+1") {
        Some(national) if national.len() == 10 => {
            format!("{}-{}-{}", &national[..3], &national[3..6], &national[6..])
        }
        _ => e164.to_string(),
    }
}

// ------------------------------------------------------- Telnyx's bodies

/// `POST /10dlc/brand` (and `PUT /10dlc/brand/{id}`) for the brand form;
/// Telnyx reports to `webhook_url`.
pub fn brand_body(brand: &TenDlcBrandDraft, webhook_url: &str) -> Value {
    let country = brand.country.as_str();
    let postal = if country == "US" {
        brand.postal_code.chars().take(5).collect()
    } else {
        brand.postal_code.clone()
    };
    let mut body = json!({
        "entityType": brand.entity_type.map(EntityType::as_str),
        "displayName": brand.display_name,
        "companyName": brand.company_name,
        "ein": tax_id(&brand.ein, country).unwrap_or_else(|| brand.ein.clone()),
        "phone": brand.phone,
        "street": brand.street,
        "city": brand.city,
        "state": brand.state,
        "postalCode": postal,
        "country": country,
        "email": brand.email,
        "website": brand.website,
        "vertical": brand.vertical.map(Vertical::as_str),
        "webhookURL": webhook_url,
    });
    for (key, value) in [
        ("firstName", &brand.first_name),
        ("lastName", &brand.last_name),
    ] {
        if !value.is_empty() {
            body[key] = json!(value);
        }
    }
    if brand.entity_type == Some(EntityType::PublicProfit) {
        body["stockSymbol"] = json!(brand.stock_symbol);
        body["stockExchange"] = json!(brand.stock_exchange.map(StockExchange::as_str));
        body["businessContactEmail"] = json!(brand.business_contact_email);
    }
    body
}

/// `POST /10dlc/campaignBuilder` for the campaign form of `brand_id`;
/// `reference_id` makes a second create of the same campaign fail (and
/// finds it again).
pub fn campaign_body(
    campaign: &TenDlcCampaignDraft,
    brand_id: &str,
    reference_id: &str,
    webhook_url: &str,
) -> Value {
    let mut body = json!({
        "brandId": brand_id,
        "referenceId": reference_id,
        "usecase": campaign.usecase.map(UseCase::as_str),
        "description": campaign.description,
        "messageFlow": campaign.message_flow,
        "helpMessage": campaign.help_message,
        "helpKeywords": campaign.help_keywords,
        "optinMessage": campaign.optin_message,
        "optinKeywords": campaign.optin_keywords,
        "optoutMessage": campaign.optout_message,
        "optoutKeywords": campaign.optout_keywords,
        "subscriberHelp": true,
        "subscriberOptin": true,
        "subscriberOptout": true,
        "embeddedLink": campaign.embedded_link,
        "embeddedPhone": campaign.embedded_phone,
        "numberPool": campaign.number_pool,
        "ageGated": campaign.age_gated,
        "directLending": campaign.direct_lending,
        "termsAndConditions": campaign.terms_accepted,
        "autoRenewal": true,
        "webhookURL": webhook_url,
    });
    if !campaign.sub_usecases.is_empty() {
        body["subUsecases"] = json!(
            campaign
                .sub_usecases
                .iter()
                .map(|sub| sub.as_str())
                .collect::<Vec<_>>()
        );
    }
    for (at, sample) in campaign.samples.iter().enumerate() {
        body[format!("sample{}", at + 1)] = json!(sample);
    }
    for (key, value) in [
        ("embeddedLinkSample", &campaign.embedded_link_sample),
        ("privacyPolicyLink", &campaign.privacy_policy_link),
        (
            "termsAndConditionsLink",
            &campaign.terms_and_conditions_link,
        ),
    ] {
        if !value.is_empty() {
            body[key] = json!(value);
        }
    }
    body
}

/// `PUT /10dlc/campaign/{id}`: what Telnyx takes on a registered campaign
/// (the samples, message flow and help message).
pub fn campaign_update_body(campaign: &TenDlcCampaignDraft, webhook_url: &str) -> Value {
    let mut body = json!({
        "messageFlow": campaign.message_flow,
        "helpMessage": campaign.help_message,
        "autoRenewal": true,
        "webhookURL": webhook_url,
    });
    for (at, sample) in campaign.samples.iter().enumerate() {
        body[format!("sample{}", at + 1)] = json!(sample);
    }
    body
}

// ------------------------------------------------------------------ rows

/// A company's registration, as stored.
#[derive(Debug, Clone, FromRow)]
pub struct Row {
    pub id: i32,
    pub company_id: i32,
    #[sqlx(try_from = "String")]
    pub status: TenDlcStatus,
    /// JSON: the form.
    pub draft: String,
    /// JSON: the brand and campaign forms as last sent.
    pub sent_brand: Option<String>,
    pub sent_campaign: Option<String>,
    pub telnyx_brand_id: Option<String>,
    pub tcr_brand_id: Option<String>,
    pub brand_status: Option<String>,
    pub brand_identity_status: Option<String>,
    pub brand_create_started_at: Option<DateTime<Utc>>,
    pub telnyx_campaign_id: Option<String>,
    pub tcr_campaign_id: Option<String>,
    pub campaign_status: Option<String>,
    pub campaign_seq: i64,
    /// JSON: Telnyx's reasons.
    pub brand_feedback: Option<String>,
    pub campaign_feedback: Option<String>,
    pub last_error: Option<String>,
    pub draft_saved_at: Option<DateTime<Utc>>,
    pub draft_saved_by_name: Option<String>,
    pub ready_at: Option<DateTime<Utc>>,
    pub ready_by_name: Option<String>,
    pub sent_at: Option<DateTime<Utc>>,
    pub sent_by_name: Option<String>,
    pub resent_at: Option<DateTime<Utc>>,
    pub approved_at: Option<DateTime<Utc>>,
    pub rejected_at: Option<DateTime<Utc>>,
    pub refreshed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub company_name: String,
}

const SELECT: &str = "SELECT r.id, r.company_id, r.status, CAST(r.draft AS CHAR) AS draft,
        CAST(r.sent_brand AS CHAR) AS sent_brand, CAST(r.sent_campaign AS CHAR) AS sent_campaign,
        r.telnyx_brand_id, r.tcr_brand_id, r.brand_status, r.brand_identity_status,
        r.brand_create_started_at, r.telnyx_campaign_id, r.tcr_campaign_id, r.campaign_status,
        CAST(r.campaign_seq AS SIGNED) AS campaign_seq,
        CAST(r.brand_feedback AS CHAR) AS brand_feedback,
        CAST(r.campaign_feedback AS CHAR) AS campaign_feedback, r.last_error,
        r.draft_saved_at, COALESCE(NULLIF(TRIM(us.name), ''), us.email) AS draft_saved_by_name,
        r.ready_at, COALESCE(NULLIF(TRIM(ur.name), ''), ur.email) AS ready_by_name,
        r.sent_at, COALESCE(NULLIF(TRIM(ut.name), ''), ut.email) AS sent_by_name,
        r.resent_at, r.approved_at, r.rejected_at, r.refreshed_at, r.created_at, r.updated_at,
        c.name AS company_name
   FROM tendlc_registrations r
   JOIN company c ON c.id = r.company_id
   LEFT JOIN users us ON us.id = r.draft_saved_by_user_id
   LEFT JOIN users ur ON ur.id = r.ready_by_user_id
   LEFT JOIN users ut ON ut.id = r.sent_by_user_id";

fn parse_feedback(json: Option<&str>) -> Vec<TenDlcFeedback> {
    json.and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default()
}

impl Row {
    /// The form as saved.
    pub fn draft(&self) -> TenDlcDraft {
        serde_json::from_str(&self.draft).unwrap_or_default()
    }

    /// The brand form as last sent; `None` before.
    pub fn sent_brand(&self) -> Option<TenDlcBrandDraft> {
        self.sent_brand
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
    }

    /// The campaign form as last sent; `None` before.
    pub fn sent_campaign(&self) -> Option<TenDlcCampaignDraft> {
        self.sent_campaign
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
    }

    /// Telnyx's reasons about the brand.
    pub fn brand_feedback(&self) -> Vec<TenDlcFeedback> {
        parse_feedback(self.brand_feedback.as_deref())
    }

    /// Telnyx's (and the carriers') reasons about the campaign.
    pub fn campaign_feedback(&self) -> Vec<TenDlcFeedback> {
        parse_feedback(self.campaign_feedback.as_deref())
    }

    /// TCR refused the brand.
    pub fn brand_failed(&self) -> bool {
        self.brand_status.as_deref() == Some(BRAND_FAILED)
    }

    /// The brand is registered (or Telnyx did not say otherwise), so its
    /// campaign can be created.
    pub fn brand_usable(&self) -> bool {
        self.telnyx_brand_id.is_some()
            && self
                .brand_status
                .as_deref()
                .is_none_or(|status| status == BRAND_OK)
    }

    /// A campaign is registered: only its samples, message flow and help
    /// message can change (a campaign TCR refused is replaced instead).
    pub fn campaign_locked(&self) -> bool {
        self.telnyx_campaign_id.is_some() && self.campaign_status.as_deref() != Some("TCR_FAILED")
    }
}

/// The company's registration.
pub async fn load(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Option<Row>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE r.company_id = ?"
    )))
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// The registrations in `statuses` (every one when empty): those waiting
/// for a superuser first, then the newest.
pub async fn list(pool: &MySqlPool, statuses: &[TenDlcStatus]) -> sqlx::Result<Vec<Row>> {
    let mut query = sqlx::QueryBuilder::new(SELECT);
    query.push(" WHERE c.deleted_at IS NULL");
    let names: Vec<String> = statuses
        .iter()
        .map(|status| status.as_str().to_string())
        .collect();
    crate::sql::push_in(&mut query, "r.status", &names);
    query.push(
        " ORDER BY FIELD(r.status, 'ready', 'rejected', 'sent', 'campaign_pending', 'draft',
                         'approved', 'suspended'),
                   r.updated_at DESC, r.id",
    );
    query.build_query_as().fetch_all(pool).await
}

/// Saves the form (making the company's registration on its first save).
pub async fn save_draft(
    pool: &MySqlPool,
    company_id: i32,
    draft: &TenDlcDraft,
    actor: i32,
) -> sqlx::Result<()> {
    let json = serde_json::to_string(draft).unwrap_or_else(|_| "{}".into());
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT IGNORE INTO tendlc_registrations (company_id, draft, created_by_user_id)
         VALUES (?, ?, ?)",
    )
    .bind(company_id)
    .bind(&json)
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE tendlc_registrations
            SET draft = ?, draft_saved_at = UTC_TIMESTAMP(), draft_saved_by_user_id = ?
          WHERE company_id = ?",
    )
    .bind(&json)
    .bind(actor)
    .bind(company_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Moves the registration from `from` to `to`, once (compare and set):
/// whether it moved. `ready` records who marked it; `approved` and
/// `rejected` when.
pub async fn move_status<'e>(
    executor: impl MySqlExecutor<'e>,
    id: i32,
    from: TenDlcStatus,
    to: TenDlcStatus,
    actor: Option<i32>,
) -> sqlx::Result<bool> {
    let moved = sqlx::query(
        "UPDATE tendlc_registrations
            SET status = ?,
                ready_at = IF(? = 'ready', UTC_TIMESTAMP(), ready_at),
                ready_by_user_id = IF(? = 'ready', ?, ready_by_user_id),
                approved_at = IF(? = 'approved', UTC_TIMESTAMP(), approved_at),
                rejected_at = IF(? = 'rejected', UTC_TIMESTAMP(), rejected_at)
          WHERE id = ? AND status = ?",
    )
    .bind(to.as_str())
    .bind(to.as_str())
    .bind(to.as_str())
    .bind(actor)
    .bind(to.as_str())
    .bind(to.as_str())
    .bind(id)
    .bind(from.as_str())
    .execute(executor)
    .await?
    .rows_affected();
    Ok(moved == 1)
}

/// Writes one entry of the phone audit log about the registration (no form
/// contents: ids, statuses and counts only).
pub async fn audit<'e>(
    executor: impl MySqlExecutor<'e>,
    company_id: i32,
    actor: Option<i32>,
    action: &str,
    registration_id: i32,
    details: &Value,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO phone_audit_log (company_id, actor_user_id, action, target, details)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(company_id)
    .bind(actor)
    .bind(action)
    .bind(format!("tendlc:{registration_id}"))
    .bind(details.to_string())
    .execute(executor)
    .await?;
    Ok(())
}

/// What the suggestion is made from: the company's name and domain, its
/// main line (else its first number), and the user's email and name.
pub async fn suggestion_for(
    pool: &MySqlPool,
    company_id: i32,
    user_email: &str,
    user_name: Option<&str>,
) -> sqlx::Result<TenDlcDraft> {
    let company: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT name, domain FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    let (name, domain) = company.unwrap_or_default();
    let line: Option<String> = sqlx::query_scalar(
        "SELECT e164 FROM telnyx_numbers WHERE company_id = ? AND active = 1
          ORDER BY kind = 'main' DESC, id LIMIT 1",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(suggestion(
        &name,
        domain.as_deref(),
        line.as_deref(),
        user_email,
        user_name,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete() -> TenDlcDraft {
        let mut draft = suggestion(
            "Acme Stone",
            Some("acmestone.com"),
            Some("+13175550100"),
            "mark@acme.test",
            Some("Mark Manager"),
        );
        draft.brand.company_name = "Acme Stone LLC".into();
        draft.brand.ein = "12-3456789".into();
        draft.brand.street = "100 Main St".into();
        draft.brand.city = "Indianapolis".into();
        draft.brand.state = "IN".into();
        draft.brand.postal_code = "46204".into();
        draft.campaign.terms_accepted = true;
        normalized(&draft)
    }

    fn fields(draft: &TenDlcDraft) -> Vec<String> {
        problems(draft)
            .into_iter()
            .map(|problem| problem.field)
            .collect()
    }

    #[test]
    fn the_suggestion_with_the_business_details_is_complete() {
        let draft = complete();
        assert_eq!(problems(&draft), Vec::<TenDlcProblem>::new());
        assert_eq!(draft.brand.website, "https://acmestone.com");
        assert_eq!(draft.brand.first_name, "Mark");
        assert!(draft.campaign.help_message.contains("317-555-0100"));
        for sample in &draft.campaign.samples {
            assert!(sample.starts_with("Acme Stone:") && sample.contains("STOP"));
            assert!(chars(sample) <= MESSAGE_CHARS.1);
        }
    }

    #[test]
    fn an_empty_form_names_every_missing_field() {
        let found = fields(&normalized(&TenDlcDraft::default()));
        for field in [
            "brand.entityType",
            "brand.displayName",
            "brand.companyName",
            "brand.country",
            "brand.ein",
            "brand.phone",
            "brand.email",
            "brand.website",
            "brand.street",
            "brand.city",
            "brand.state",
            "brand.postalCode",
            "brand.vertical",
            "campaign.usecase",
            "campaign.description",
            "campaign.messageFlow",
            "campaign.samples",
            "campaign.helpMessage",
            "campaign.helpKeywords",
            "campaign.optinMessage",
            "campaign.optinKeywords",
            "campaign.optoutMessage",
            "campaign.optoutKeywords",
            "campaign.termsAccepted",
        ] {
            assert!(found.contains(&field.to_string()), "{field}: {found:?}");
        }
    }

    #[test]
    fn samples_name_the_business_and_say_how_to_opt_out() {
        let mut draft = complete();
        draft.campaign.samples = vec![
            "Your quote is ready, call us any time. Reply STOP to opt out.".into(),
            "Acme Stone: your install is Friday at 9 AM, see you then!".into(),
            "Acme Stone: track it at https://bit.ly/x1 Reply STOP to opt out.".into(),
            "Acme Stone: call 317-555-0199 to book. Reply STOP to opt out.".into(),
            "acme stone: CALL 317-555-0199 to book. reply stop to opt out.".into(),
        ];
        let found = problems(&normalized(&draft));
        let messages: Vec<(&str, &str)> = found
            .iter()
            .map(|problem| (problem.field.as_str(), problem.message.as_str()))
            .collect();
        assert_eq!(
            messages,
            [
                (
                    "campaign.samples.0",
                    "Sample text 1 must name the business."
                ),
                (
                    "campaign.samples.1",
                    "Sample text 2 must say how to opt out, like \"Reply STOP to opt out\"."
                ),
                (
                    "campaign.samples.2",
                    "Sample text 3 uses a link shortener: carriers refuse them. Use the full link."
                ),
                (
                    "campaign.samples.4",
                    "Sample text 5 repeats another: make each different."
                ),
                (
                    "campaign.embeddedLink",
                    "A sample text has a link: say the texts carry links."
                ),
                (
                    "campaign.embeddedPhone",
                    "A sample text has a phone number: say the texts carry phone numbers."
                ),
            ]
        );
    }

    #[test]
    fn tax_ids_keywords_and_public_companies_are_checked() {
        assert_eq!(tax_id("123456789", "US").as_deref(), Some("12-3456789"));
        assert_eq!(tax_id(" 12-3456789 ", "US").as_deref(), Some("12-3456789"));
        assert_eq!(tax_id("1234-56789", "US"), None);
        assert_eq!(tax_id("12345678", "US"), None);
        assert_eq!(tax_id("123 456 789", "CA").as_deref(), Some("123456789"));
        assert_eq!(
            tax_id("123456789rt0001", "CA").as_deref(),
            Some("123456789RT0001")
        );
        assert_eq!(keywords(" stop, Quit ,stop end"), ["STOP", "QUIT", "END"]);
        let mut draft = complete();
        draft.campaign.optout_keywords = "QUIT,END".into();
        draft.campaign.help_keywords = "HELP,HE LP!".into();
        draft.brand.entity_type = Some(EntityType::PublicProfit);
        let found = fields(&normalized(&draft));
        assert_eq!(
            found,
            [
                "brand.stockSymbol",
                "brand.stockExchange",
                "brand.businessContactEmail",
                "campaign.helpKeywords",
                "campaign.optoutKeywords"
            ]
        );
    }

    #[test]
    fn mixed_campaigns_need_two_to_five_kinds() {
        let mut draft = complete();
        draft.campaign.sub_usecases = vec![UseCase::Marketing];
        assert_eq!(fields(&normalized(&draft)), ["campaign.subUsecases"]);
        draft.campaign.usecase = Some(UseCase::LowVolume);
        assert!(problems(&normalized(&draft)).is_empty());
        // Other use cases have none: they are dropped.
        draft.campaign.usecase = Some(UseCase::CustomerCare);
        assert!(normalized(&draft).campaign.sub_usecases.is_empty());
    }

    #[test]
    fn telnyx_gets_its_field_names() {
        let draft = complete();
        let brand = brand_body(&draft.brand, "https://api.test/hook/10dlc");
        assert_eq!(brand["entityType"], "PRIVATE_PROFIT");
        assert_eq!(brand["ein"], "12-3456789");
        assert_eq!(brand["vertical"], "CONSTRUCTION");
        assert_eq!(brand["webhookURL"], "https://api.test/hook/10dlc");
        assert!(brand.get("stockSymbol").is_none());
        let campaign = campaign_body(&draft.campaign, "b-1", "granite-c1-r1-1", "h");
        assert_eq!(campaign["usecase"], "MIXED");
        assert_eq!(
            campaign["subUsecases"],
            json!(["CUSTOMER_CARE", "ACCOUNT_NOTIFICATION", "MARKETING"])
        );
        assert_eq!(campaign["sample3"], json!(draft.campaign.samples[2]));
        assert!(campaign.get("sample4").is_none());
        assert_eq!(campaign["termsAndConditions"], true);
        assert_eq!(campaign["referenceId"], "granite-c1-r1-1");
        let update = campaign_update_body(&draft.campaign, "h");
        assert!(update.get("usecase").is_none());
        assert_eq!(update["sample1"], json!(draft.campaign.samples[0]));
    }

    #[test]
    fn only_samples_flow_and_help_change_on_a_registered_campaign() {
        let sent = complete().campaign;
        let mut now = sent.clone();
        now.samples[0] = "Acme Stone: new text. Reply STOP to opt out.".into();
        now.help_message = "Acme Stone help: call us.".into();
        assert!(!locked_fields_changed(&sent, &now));
        assert!(editable_fields_changed(&sent, &now));
        now.usecase = Some(UseCase::CustomerCare);
        assert!(locked_fields_changed(&sent, &now));
    }
}
