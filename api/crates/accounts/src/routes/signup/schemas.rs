use crate::billing::catalog::{PlanChoice, Quote};
use crate::entitlements::AddonKey;
use crate::error::{ApiError, ApiResult};
use crate::openapi::{IntoParams, ToSchema};
use crate::routes::users::manage::checked_email;
use crate::signup::consent::{ConsentAcceptance, RenderedConsent};
use crate::signup::intent::{CompanyDetails, SignupStatus};
use crate::signup::payment::SavedMethod;
use crate::signup::provision::BankVerification;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// Query of `GET /v1/public/plans`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PlansQuery {
    /// Estimated active users, 1 to 500 (default 5; out-of-range values are
    /// clamped, unreadable ones fall back to the default).
    pub users: Option<String>,
    /// Chosen add-ons (repeat the parameter): `signatures`, `quotes` or
    /// `quickbooks`. Unknown values are ignored.
    #[serde(default)]
    pub addon: Vec<String>,
}

/// The base plan.
#[derive(Debug, Serialize, ToSchema)]
pub struct BasePlan {
    /// Always `Base plan`.
    pub name: String,
    /// Monthly price in cents, before sales tax.
    pub monthly_cents: i64,
    /// Active users the base price covers.
    pub included_users: i64,
}

/// A paid add-on of the catalog.
#[derive(Debug, Serialize, ToSchema)]
pub struct AddonInfo {
    pub key: AddonKey,
    /// Name people see, e.g. `QuickBooks`.
    pub name: String,
    /// One line: what it unlocks.
    pub description: String,
    /// Monthly price in cents, before sales tax.
    pub monthly_cents: i64,
}

/// The catalog and a quote for one choice.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlansResponse {
    pub base: BasePlan,
    /// Monthly price in cents of each active user above the included ones.
    pub extra_user_cents: i64,
    /// Months without a charge after signup.
    pub free_months: u32,
    /// Smallest user estimate.
    pub min_users: i64,
    /// Largest user estimate.
    pub max_users: i64,
    /// Every add-on.
    pub addons: Vec<AddonInfo>,
    /// The price of the requested choice.
    pub quote: Quote,
    /// False while self-serve signup is switched off: every
    /// `/v1/public/signups*` call answers 503 `signup_unavailable`, so show
    /// "paused" instead of the form. The plans are listed either way.
    pub signups_enabled: bool,
}

/// The owner of the new company, who signs in with these.
#[derive(Deserialize, ToSchema)]
pub struct OwnerDetails {
    /// Full name, 2 to 80 characters: starts with a letter, then letters,
    /// spaces, `'`, `’`, `.` and `-`.
    pub name: String,
    /// Email, up to 254 bytes: an ASCII local part and a dotted domain with
    /// an alphabetic top-level domain. Receives the code; used to sign in.
    pub email: String,
    /// Password: at least 10 characters and at most 200 bytes (as sign-in
    /// counts them; surrounding spaces are dropped), not the email.
    pub password: String,
}

/// Body of `POST /v1/public/signups`.
#[derive(Deserialize, ToSchema)]
pub struct CreateSignupBody {
    /// What the buyer picked in step 1.
    pub plan: PlanChoice,
    pub company: CompanyDetails,
    pub owner: OwnerDetails,
}

/// Answer of `POST /v1/public/signups`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedSignup {
    /// The signup id. Keep it private: it is the only secret of the flow.
    pub id: String,
    /// Seconds until another code can be requested.
    pub resend_in_seconds: i64,
}

/// Answer of `POST /v1/public/signups/{id}/code`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ResentCode {
    /// Seconds until another code can be requested.
    pub resend_in_seconds: i64,
}

/// Body of `POST /v1/public/signups/{id}/verification`.
#[derive(Deserialize, ToSchema)]
pub struct VerifyBody {
    /// The 6-digit code from the email.
    pub code: String,
}

/// Where the card or bank account is collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaymentMode {
    /// The api attaches a test payment method itself (sandbox).
    Test,
    /// The Payment Element collects one.
    Stripe,
}

/// A signup in progress.
#[derive(Debug, Serialize, ToSchema)]
pub struct Signup {
    /// The signup id (the flow's secret).
    pub id: String,
    pub status: SignupStatus,
    /// The email with its local part hidden, e.g. `a***@acme.com`.
    pub email_masked: String,
    /// The company name from step 2.
    pub company_name: String,
    pub plan: PlanChoice,
    /// The price of `plan` today.
    pub quote: Quote,
    pub payment_mode: PaymentMode,
    /// The Stripe publishable key for the Payment Element; only in `stripe`
    /// mode, else null.
    pub publishable_key: Option<String>,
    /// The payment method already set up, read live from Stripe: only while
    /// `payment_ready` or `awaiting_bank_verification`. Null when nothing
    /// usable is saved or Stripe cannot be read right now.
    pub payment_method: Option<SavedMethod>,
    /// Seconds until another code can be requested (0 = now).
    pub resend_in_seconds: i64,
}

/// Answer of `GET /v1/public/signups/{id}/consents`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConsentsResponse {
    /// The checkboxes, in the order to show them.
    pub consents: Vec<RenderedConsent>,
}

/// Answer of `POST /v1/public/signups/{id}/payment-setup`:
/// `{"mode": "test"}` or `{"mode": "stripe", "client_secret", "publishable_key"}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PaymentSetup {
    /// `test`: a test payment method was attached, nothing for the page to
    /// do. `stripe`: confirm the SetupIntent with the Payment Element.
    pub mode: PaymentMode,
    /// The SetupIntent's client secret; only in `stripe` mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// The Stripe publishable key; only in `stripe` mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publishable_key: Option<String>,
}

/// Body of `POST /v1/public/signups/{id}/completion`.
#[derive(Deserialize, ToSchema)]
pub struct CompletionBody {
    /// Every checkbox the buyer ticked, as `GET .../consents` showed it.
    pub consents: Vec<ConsentAcceptance>,
}

/// Answer of `POST /v1/public/signups/{id}/completion`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Completion {
    /// Where the browser goes next.
    pub redirect_url: String,
    /// Set while the company waits for its bank account's micro-deposits to
    /// be verified (status `awaiting_bank_verification`): show the verify
    /// panel. Null once the signup is completed.
    pub bank_verification: Option<BankVerification>,
    /// True when this response signed the owner in (it sets the session
    /// cookie); false when an earlier completion already did, so a retry
    /// does not start a second session: send the owner to sign in.
    pub signed_in: bool,
}

/// A request that passed validation.
pub struct ValidCreate {
    pub plan: PlanChoice,
    pub company: CompanyDetails,
    pub owner_name: String,
    pub email: String,
    pub password: String,
}

/// The longest password sign-in accepts (`POST /v1/auth/login`), in bytes.
const MAX_PASSWORD_BYTES: usize = 200;

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}

fn required(value: &str, label: &str, max: usize) -> ApiResult<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(bad(&format!("Enter the {label}.")));
    }
    if value.chars().count() > max {
        return Err(bad(&format!("The {label} can be up to {max} characters.")));
    }
    if value.chars().any(char::is_control) {
        return Err(bad(&format!(
            "The {label} has a character we cannot accept."
        )));
    }
    Ok(value.to_string())
}

fn valid_zip(zip: &str) -> bool {
    let digits =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    match zip.split_once('-') {
        Some((five, four)) => digits(five, 5) && digits(four, 4),
        None => digits(zip, 5),
    }
}

/// The 50 states and DC.
const US_STATES: [&str; 51] = [
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "DC", "FL", "GA", "HI", "ID", "IL", "IN", "IA",
    "KS", "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM",
    "NY", "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA",
    "WV", "WI", "WY",
];

/// Why a phone is not a dialable US/Canada number (`+1` and 10 digits).
#[derive(Debug, PartialEq, Eq)]
enum PhoneProblem {
    Format,
    /// Starts with 0 or 1, or is N11.
    Area,
    /// The exchange starts with 0 or 1.
    Exchange,
}

fn phone_problem(phone: &str) -> Option<PhoneProblem> {
    let digits = phone
        .strip_prefix("+1")
        .filter(|digits| digits.len() == 10 && digits.bytes().all(|b| b.is_ascii_digit()));
    let Some([a, b, c, exchange, ..]) = digits.map(str::as_bytes) else {
        return Some(PhoneProblem::Format);
    };
    if matches!(a, b'0' | b'1') || (*b == b'1' && *c == b'1') {
        return Some(PhoneProblem::Area);
    }
    matches!(exchange, b'0' | b'1').then_some(PhoneProblem::Exchange)
}

/// As the payments app checks them (`companyForm.ts`).
static NAME_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\p{L}[\p{L}\p{M}\s'’.-]*$").expect("valid regex"));
static CITY_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{L}\p{M}][\p{L}\p{M}\s'’.-]*$").expect("valid regex"));
static STREET_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{L}\p{M}\p{N}\s#/.,'’&-]+$").expect("valid regex"));
static URL_SCHEME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.-]*://").expect("valid regex"));
static WEB_HOST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[a-z0-9-]+(\.[a-z0-9-]+)*\.[a-z]{2,}$").expect("valid regex")
});

/// `acme.com` or `https://www.acme.com/about` as an http(s) URL whose host
/// is a domain name, or `None` (`javascript:x`, `ftp://...`, an IP).
fn website(site: &str) -> Option<String> {
    if site.is_empty() || site.contains(char::is_whitespace) {
        return None;
    }
    let with_scheme = if URL_SCHEME.is_match(site) {
        site.to_string()
    } else {
        format!("https://{site}")
    };
    let url = url::Url::parse(&with_scheme).ok()?;
    let web = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| WEB_HOST.is_match(host))
        && with_scheme.chars().count() <= 200;
    web.then_some(with_scheme)
}

/// Stricter than the shared email check: an ASCII local part (1 to 64,
/// no leading, trailing or double dot) and a dotted domain of ASCII labels
/// with an alphabetic top-level domain.
fn signup_email(email: &str) -> bool {
    let mut parts = email.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let local_ok = (1..=64).contains(&local.len())
        && local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$&'*+/=?^_`{|}~.-".contains(c))
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..");
    let labels: Vec<&str> = domain.split('.').collect();
    let labels_ok = labels.len() >= 2
        && labels.iter().all(|label| {
            (1..=63).contains(&label.len())
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        });
    let tld_ok = labels
        .last()
        .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()));
    local_ok && labels_ok && tld_ok
}

/// Runs of whitespace as one space, trimmed (as the payments app sends it).
fn clean(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl CreateSignupBody {
    /// Checks and normalizes every field before anything is written, with
    /// the payments app's rules (`apps/payments/src/lib/companyForm.ts`).
    pub fn validate(self) -> ApiResult<ValidCreate> {
        let company = self.company;
        let name = required(&clean(&company.name), "company name", 100)?;
        if name.chars().count() < 2 {
            return Err(bad("Enter the full company name."));
        }
        let street = required(&clean(&company.street), "street address", 100)?;
        if street.chars().count() < 3 || !street.chars().any(char::is_alphabetic) {
            return Err(bad("Enter the street and number, like 1 Main St."));
        }
        if !STREET_CHARS.is_match(&street) {
            return Err(bad(
                "The street address can use letters, numbers, spaces and # / - . , ' & only.",
            ));
        }
        let city = required(&clean(&company.city), "city", 50)?;
        if !CITY_CHARS.is_match(&city) {
            return Err(bad(
                "The city can use letters, spaces and ' - . only, like St. Louis.",
            ));
        }
        if city.chars().count() < 2 {
            return Err(bad("Enter the full city name."));
        }
        let state = company.state.trim().to_ascii_uppercase();
        if !US_STATES.contains(&state.as_str()) {
            return Err(bad("Enter a two-letter state, like IN."));
        }
        let zip = company.zip.trim().to_string();
        if !valid_zip(&zip) {
            return Err(bad("Enter a ZIP code like 46204 or 46204-1234."));
        }
        let phone = company.phone.trim().to_string();
        match phone_problem(&phone) {
            None => {}
            Some(PhoneProblem::Format) => {
                return Err(bad(
                    "Enter a phone number with the country code, like +13175550100.",
                ));
            }
            Some(PhoneProblem::Area) => {
                return Err(bad("Enter a phone number with a real area code, like 317."));
            }
            Some(PhoneProblem::Exchange) => {
                return Err(bad("Enter a valid phone number, like +13175550100."));
            }
        }
        let website = match company.website.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(site) => {
                Some(website(site).ok_or_else(|| bad("Enter a valid website or leave it empty."))?)
            }
        };
        let owner_name = required(&clean(&self.owner.name), "owner name", 80)?;
        if !NAME_CHARS.is_match(&owner_name) {
            return Err(bad(
                "The owner name can use letters, spaces and ' - . only.",
            ));
        }
        if owner_name.chars().count() < 2 {
            return Err(bad("Enter the owner's full name."));
        }
        let email = checked_email(&self.owner.email)?;
        // Bytes, as sign-in counts them.
        if email.len() > 254 {
            return Err(bad("The email can be up to 254 characters."));
        }
        if !signup_email(&email) {
            return Err(bad("Enter a valid email"));
        }
        let password = self.owner.password.trim().to_string();
        if password.chars().count() < 10 {
            return Err(bad("Password must be at least 10 characters long."));
        }
        // Bytes, as sign-in counts them: a longer one could never sign in.
        if password.len() > MAX_PASSWORD_BYTES {
            return Err(bad(
                "Password is too long: use up to 200 letters, fewer with accents or symbols.",
            ));
        }
        if password.contains('\0') {
            return Err(bad("Password has a character we cannot accept."));
        }
        if password.to_lowercase() == email {
            return Err(bad("Use a password that is different from your email."));
        }
        Ok(ValidCreate {
            plan: PlanChoice::normalized(self.plan.users, &self.plan.addons),
            company: CompanyDetails {
                name,
                phone,
                website,
                street,
                city,
                state,
                zip,
            },
            owner_name,
            email,
            password,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(edit: impl FnOnce(&mut CreateSignupBody)) -> CreateSignupBody {
        let mut body = CreateSignupBody {
            plan: PlanChoice {
                users: 5,
                addons: Vec::new(),
            },
            company: CompanyDetails {
                name: "Acme Stone".into(),
                phone: "+13175550100".into(),
                website: None,
                street: "1 Main St".into(),
                city: "Indianapolis".into(),
                state: "in".into(),
                zip: "46204".into(),
            },
            owner: OwnerDetails {
                name: "Ann Owner".into(),
                email: " Ann@Acme.test ".into(),
                password: "correct horse".into(),
            },
        };
        edit(&mut body);
        body
    }

    fn message(result: ApiResult<ValidCreate>) -> String {
        match result {
            Err(ApiError::BadRequest(message)) => message,
            Err(other) => panic!("unexpected error {other}"),
            Ok(_) => panic!("expected a 400"),
        }
    }

    #[test]
    fn a_good_request_is_normalized() {
        let valid = body(|_| {}).validate().ok().unwrap();
        assert_eq!(valid.email, "ann@acme.test");
        assert_eq!(valid.company.state, "IN");
    }

    #[test]
    fn bad_fields_are_named() {
        assert_eq!(
            message(body(|b| b.owner.password = "123456789".into()).validate()),
            "Password must be at least 10 characters long."
        );
        assert!(message(body(|b| b.company.zip = "4620".into()).validate()).contains("ZIP"));
        assert!(message(body(|b| b.company.phone = "317".into()).validate()).contains("phone"));
        assert!(message(body(|b| b.company.state = "IND".into()).validate()).contains("state"));
        assert!(
            message(body(|b| b.company.name = "  ".into()).validate()).contains("company name")
        );
        assert!(message(body(|b| b.owner.email = "nope".into()).validate()).contains("email"));
        assert!(
            body(|b| b.company.zip = "46204-1234".into())
                .validate()
                .is_ok()
        );
    }

    /// The good request with one field set to `value`.
    fn with(field: &str, value: &str) -> CreateSignupBody {
        body(|b| {
            let value = value.to_string();
            match field {
                "name" => b.company.name = value,
                "phone" => b.company.phone = value,
                "website" => b.company.website = Some(value),
                "street" => b.company.street = value,
                "city" => b.company.city = value,
                "state" => b.company.state = value,
                "zip" => b.company.zip = value,
                "owner" => b.owner.name = value,
                "email" => b.owner.email = value,
                "password" => b.owner.password = value,
                _ => panic!("unknown field {field}"),
            }
        })
    }

    /// The payments app's rules (`usValidation.ts`, `companyForm.ts`).
    #[test]
    fn the_client_rules_hold_on_the_server() {
        let long = |n: usize| "a".repeat(n);
        let refused = [
            ("phone", "+443175550100".into(), "country code"),
            ("phone", "+1317555010".into(), "country code"),
            ("phone", "+131755501000".into(), "country code"),
            ("phone", "3175550100".into(), "country code"),
            ("phone", "+10175550100".into(), "area code"),
            ("phone", "+11175550100".into(), "area code"),
            ("phone", "+19115550100".into(), "area code"),
            ("phone", "+13170550100".into(), "valid phone"),
            ("phone", "+13171550100".into(), "valid phone"),
            ("state", "XX".into(), "state"),
            ("state", "PR".into(), "state"),
            ("zip", "4620A".into(), "ZIP"),
            ("zip", "46204-123".into(), "ZIP"),
            (
                "street",
                format!("1 {}", long(99)),
                "street address can be up to 100",
            ),
            ("street", "1A".into(), "street and number"),
            ("street", "1234".into(), "street and number"),
            ("street", "1 Main St <b>".into(), "street address can use"),
            ("city", long(51), "city can be up to 50"),
            ("city", "Indy 500".into(), "city can use"),
            ("city", ".Indy".into(), "city can use"),
            ("city", "I".into(), "full city"),
            ("owner", long(81), "owner name can be up to 80"),
            ("owner", "Ann 2".into(), "owner name can use"),
            ("owner", "-Ann".into(), "owner name can use"),
            ("owner", "A".into(), "full name"),
            ("name", "A".into(), "full company"),
            ("name", long(101), "company name can be up to 100"),
            ("name", "Acme\u{7}".into(), "cannot accept"),
            ("email", "j\u{f6}rg@acme.test".into(), "email"),
            ("email", "ann@acm\u{e9}.test".into(), "email"),
            ("email", "ann@acme.123".into(), "email"),
            ("email", "ann@acme.t".into(), "email"),
            ("email", "ann..b@acme.test".into(), "email"),
            ("email", ".ann@acme.test".into(), "email"),
            ("email", "ann@-acme.test".into(), "email"),
            ("email", format!("{}@acme.test", long(65)), "email"),
            ("website", "javascript:x".into(), "website"),
            ("website", "javascript://acme.com".into(), "website"),
            ("website", "ftp://acme.com".into(), "website"),
            ("website", "http://10.0.0.1".into(), "website"),
            ("website", "localhost".into(), "website"),
            ("website", format!("acme.com/{}", long(200)), "website"),
            (
                "password",
                "ANN@acme.test".into(),
                "different from your email",
            ),
        ];
        for (field, value, needle) in refused {
            let text: String = message(with(field, &value).validate());
            assert!(text.contains(needle), "{field} {value:?}: {text}");
        }

        let accepted = [
            ("state", "dc".to_string()),
            ("zip", "20001-1234".into()),
            ("street", "12 O\u{2019}Neil Ave., Suite #4/B & Co-op".into()),
            ("street", format!("1 {}", long(98))),
            ("city", "Winston-Salem".into()),
            ("city", "St. Louis".into()),
            ("city", "Coeur d'Al\u{e8}ne".into()),
            ("owner", "Mary-Jane O'Neil Jr.".into()),
            ("owner", long(80)),
            ("email", "first.last+tag@sub.acme.co".into()),
            ("website", "acme.com".into()),
            ("website", "http://www.acme.com/about".into()),
            ("phone", "+12125550100".into()),
        ];
        for (field, value) in accepted {
            assert!(with(field, &value).validate().is_ok(), "{field} {value:?}");
        }
    }

    #[test]
    fn text_is_cleaned_and_the_website_gets_a_scheme() {
        let valid = body(|b| {
            b.company.name = "  Acme   Stone ".into();
            b.company.website = Some(" acme.com ".into());
        })
        .validate()
        .ok()
        .unwrap();
        assert_eq!(valid.company.name, "Acme Stone");
        assert_eq!(valid.company.website.as_deref(), Some("https://acme.com"));
    }
}
