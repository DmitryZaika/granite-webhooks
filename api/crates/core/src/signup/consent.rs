//! The consent catalog and its records (02-architecture.md section 9,
//! 04-legal.md sections 6.1 and 7).
//!
//! The server owns the checkbox texts: `GET .../consents` renders them with
//! the signup's facts, the page shows exactly that text, and completion sends
//! back `{kind, version, text_sha256}`. The server renders again, compares,
//! and stores one `consent_records` row per checkbox. A stale page (prices or
//! versions changed) gets 409 `consents_changed`.

use super::RequestMeta;
use super::intent::Intent;
use crate::billing::catalog::{
    Catalog, INCLUDED_USERS, Quote, QuoteLineKind, onboarding_deadline, signup_day, trial_end,
};
use crate::openapi::ToSchema;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::MySqlConnection;

/// Legal documents, copied from general_datebase
/// `app/content/legal/manifest.json` (a new version is a new pair here).
pub const TERMS_VERSION: &str = "terms-2026-10-v3";
pub const TERMS_SHA256: &str = "b68add71c84afe50bf05c9c233b3c03009bbacf849af5a71cfdc0c60371ffc9e";
pub const PRIVACY_VERSION: &str = "privacy-2026-10-v2";
pub const PRIVACY_SHA256: &str = "a5c1a5ad7793ad455a709c474429fc1b5ab45829e2bfafd3249a5c90ecd9a2dc";
pub const DPA_VERSION: &str = "dpa-2026-10-v1";
pub const DPA_SHA256: &str = "a41370b6f1d617ea63ac1a01918d1bc38d88db13c5f2a62e71f7063c53184dc4";

// Each checkbox version is pinned to the SHA-256 of its text rendered with
// the unit tests' fixed facts (`every_checkbox_version_pins_its_wording`):
// a wording change needs a new version and a new pin together.
#[cfg(test)]
const TERMS_PIN: &str = "d83a250c182010627a751581496a8ac3be9db1f86437eaa8ca74476d2dc6aa1d";

/// A checkbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConsentKind {
    /// Terms of Service, Subscription Terms, DPA and Privacy Policy.
    Terms,
    /// The free period, then a monthly charge until cancelled.
    AutoRenewal,
    /// $30 per active user above 10.
    PerUserPricing,
    /// The onboarding advance: a month paid in advance with the next charge
    /// if onboarding (free) is not completed in 30 days.
    OnboardingAdvance,
    /// The ACH debit authorization, when paying by bank account.
    AchAuthorization,
    /// The card authorization, when paying by card.
    CardAuthorization,
}

impl ConsentKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Terms => "terms",
            Self::AutoRenewal => "auto_renewal",
            Self::PerUserPricing => "per_user_pricing",
            Self::OnboardingAdvance => "onboarding_advance",
            Self::AchAuthorization => "ach_authorization",
            Self::CardAuthorization => "card_authorization",
        }
    }
}

/// How a payment method pays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaymentMethodType {
    /// A credit or debit card.
    Card,
    /// A US bank account (ACH debit).
    UsBankAccount,
}

impl PaymentMethodType {
    /// Stripe's `type` of the payment method.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Card => "card",
            Self::UsBankAccount => "us_bank_account",
        }
    }

    /// Stripe's `type`; `None` for any other kind of payment method.
    pub fn parse(value: &str) -> Option<Self> {
        [Self::Card, Self::UsBankAccount]
            .into_iter()
            .find(|kind| kind.as_str() == value)
    }
}

/// A checkbox the buyer ticked, as sent back on completion.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct ConsentAcceptance {
    pub kind: ConsentKind,
    /// The `version` the page showed.
    pub version: String,
    /// The `sha256` the page showed.
    pub text_sha256: String,
}

/// A document a checkbox refers to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct DocumentLink {
    /// e.g. "Terms of Service".
    pub title: String,
    /// The exact version on the main app (`/terms?v=<version>`,
    /// `/privacy-policy?v=<version>`, `/dpa?v=<version>`).
    pub url: String,
}

/// A checkbox text rendered for one signup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RenderedConsent {
    pub kind: ConsentKind,
    /// Version of this checkbox text, e.g. `auto-renewal-2026-10-v1`.
    pub version: &'static str,
    /// The checkbox label, shown verbatim.
    pub text: String,
    /// SHA-256 (hex) of `text`; send it back on completion.
    pub sha256: String,
    /// Documents the text refers to, linked under it.
    pub documents: Vec<DocumentLink>,
    /// The payment method this checkbox is for (`card` or
    /// `us_bank_account`): show it only while that kind of method is
    /// selected or saved. Null: shown for every method.
    pub payment_method_type: Option<PaymentMethodType>,
    /// `consent_records.document_version`.
    #[serde(skip)]
    pub document_version: &'static str,
    /// `consent_records.document_sha256`, or none.
    #[serde(skip)]
    pub document_sha256: Option<&'static str>,
}

impl RenderedConsent {
    /// Required (and recorded) when paying with `method`.
    pub fn applies_to(&self, method: PaymentMethodType) -> bool {
        self.payment_method_type.is_none_or(|kind| kind == method)
    }
}

/// Which checkboxes a flow shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentContext {
    /// Self-serve signup: terms, auto_renewal, per_user_pricing,
    /// onboarding_advance, then card_authorization and ach_authorization.
    Signup,
    /// A company moved onto a plan by Granite Manager (plan invite): terms,
    /// auto_renewal, per_user_pricing, card_authorization and
    /// ach_authorization. No onboarding commitment.
    Invite,
}

/// What the texts and records state: computed by the server, never sent by
/// the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentFacts {
    pub company_name: String,
    /// For an invite: the real billable users, and `first_charge_on` is the
    /// invite's first charge day.
    pub quote: Quote,
    /// When the free period ends (UTC).
    pub trial_end_at: NaiveDateTime,
    /// When the onboarding deadline passes (UTC); signups only.
    pub deadline_at: Option<NaiveDateTime>,
    /// The price of each user above the included ones, in cents.
    pub extra_user_cents: i64,
}

/// The facts of `intent` at `now`, priced from `catalog`.
pub fn facts(catalog: &Catalog, intent: &Intent, now: DateTime<Utc>) -> ConsentFacts {
    ConsentFacts {
        company_name: intent.company.name.clone(),
        quote: catalog.quote(&intent.plan, signup_day(now)),
        trial_end_at: intent.trial_end_at.unwrap_or_else(|| trial_end(now)),
        deadline_at: Some(
            intent
                .deadline_at
                .unwrap_or_else(|| onboarding_deadline(now)),
        ),
        extra_user_cents: catalog.extra_user.unit_cents,
    }
}

/// What the current consent texts state as fixed amounts: they must equal
/// the catalog's, or the texts would promise a price Stripe does not charge.
pub const STATED_EXTRA_USER_CENTS: i64 = 3_000;
pub const STATED_INCLUDED_USERS: i64 = 10;

/// Why the catalog no longer matches the consent texts, or `None`.
pub fn catalog_mismatch(catalog: &Catalog) -> Option<String> {
    let mut problems = Vec::new();
    if catalog.extra_user.unit_cents != STATED_EXTRA_USER_CENTS {
        problems.push(format!(
            "the extra-user price is {} but the consent texts say {}",
            money(catalog.extra_user.unit_cents),
            money(STATED_EXTRA_USER_CENTS)
        ));
    }
    if INCLUDED_USERS != STATED_INCLUDED_USERS {
        problems.push(format!(
            "the base plan includes {INCLUDED_USERS} users but the consent texts say {STATED_INCLUDED_USERS}"
        ));
    }
    (!problems.is_empty()).then(|| problems.join("; "))
}

/// The documents a consent row records: the terms checkbox accepts three,
/// one row each (04-legal.md section 7).
fn recorded_documents(consent: &RenderedConsent) -> Vec<(&'static str, Option<&'static str>)> {
    if consent.kind == ConsentKind::Terms {
        vec![
            (TERMS_VERSION, Some(TERMS_SHA256)),
            (PRIVACY_VERSION, Some(PRIVACY_SHA256)),
            (DPA_VERSION, Some(DPA_SHA256)),
        ]
    } else {
        vec![(consent.document_version, consent.document_sha256)]
    }
}

/// The onboarding advance is charged no sooner than this after its notice
/// (at the deadline at the earliest).
pub const ONBOARDING_ADVANCE_NOTICE_DAYS: u64 = 7;

/// The day the advance would be charged if onboarding is not completed by
/// the deadline: the first charge, or later when that is under 7 days after
/// the deadline. (The live date is fixed at the notice.)
pub fn advance_charge_on(quote: &Quote) -> NaiveDate {
    quote
        .first_charge_on
        .max(quote.deadline_on + chrono::Days::new(ONBOARDING_ADVANCE_NOTICE_DAYS))
}

pub const AUTO_RENEWAL_VERSION: &str = "auto-renewal-2026-10-v2";
pub const PER_USER_PRICING_VERSION: &str = "per-user-pricing-2026-10-v1";
pub const ONBOARDING_ADVANCE_VERSION: &str = "onboarding-advance-2026-10-v2";
#[cfg(test)]
const AUTO_RENEWAL_PIN: &str = "51bd662ef0f90fbee926cf3b346de2dec1b09d9902962b379090a69e55830909";
#[cfg(test)]
const PER_USER_PRICING_PIN: &str =
    "f301281fd7dfdefa08fc9b2330f46800a0b5929a2600458197dddbfd195fa201";
#[cfg(test)]
const ONBOARDING_ADVANCE_PIN: &str =
    "b9ea66e0412707a55076758fad336ce6d12c2ef3d33f5455a92905c6c928c96b";

/// Version of [`CARD_AUTHORIZATION_TEXT`].
pub const CARD_AUTHORIZATION_VERSION: &str = "card-authorization-2026-10-v1";
#[cfg(test)]
const CARD_AUTHORIZATION_PIN: &str =
    "2be51453b129d416f835fc3bed60ee24937996c2bfc34e5a6a678c335b905fb4";
/// The card authorization (04-legal.md section 6.2, "Card consent"), ticked
/// when paying by card. The same for signups and plan invites.
pub const CARD_AUTHORIZATION_TEXT: &str = "I authorize Granite Manager and Stripe to charge \
     this card for the amounts and dates in the checkboxes above and to save it for future \
     charges until I cancel.";

/// Version of the ACH debit authorization ([`ach_authorization_text`]).
pub const ACH_AUTHORIZATION_VERSION: &str = "ach-authorization-2026-10-v2";
/// A signup's and an invite's (no (c)).
#[cfg(test)]
const ACH_AUTHORIZATION_PINS: [&str; 2] = [
    "b77b3aa1c0df2ddeb7782bdee2d9b1c9896aef5d61946f8b77f8a2a352c30eac",
    "fad489d7d5e00833da2a70fe51009c81a742d7b5299dc704b5128427d7c4af73",
];
/// The fixed end of the ACH debit authorization (04-legal.md section 6.2).
///
/// Retries (Stripe retries a debit returned for insufficient funds at most
/// twice within 40 days), revocation, the Nacha clause for business
/// accounts, and the copy on request.
pub const ACH_AUTHORIZATION_TERMS: &str = "If a debit is returned for insufficient funds, I \
     authorize Granite Manager to retry it up to 2 times within 40 days, as Nacha rules allow. \
     This authorization stays in effect until I revoke it by replacing the bank account on the \
     account page or by emailing support@granite-manager.com, at least 3 business days before \
     the next debit. If this is a business account, I agree to be bound by the Nacha Operating \
     Rules for these debits. I confirm that I am authorized to debit this account. I can ask \
     for a copy of this authorization at support@granite-manager.com.";

/// The ACH debit authorization of `context` (04-legal.md section 6.2).
///
/// It names "the bank account I add on this page": its last 4 and bank are
/// unknown until the account is linked, so they are recorded with the
/// acceptance (`amounts_and_dates`, `payment_method_id`,
/// `stripe_mandate_id`). The advance payment debit, (c), is a signup's only.
pub fn ach_authorization_text(context: ConsentContext, facts: &ConsentFacts) -> String {
    let quote = &facts.quote;
    let monthly = format!(
        "(a) {} per month plus $30 for each active user above 10, plus applicable sales tax, on \
         {} and then every month on the same day as the first charge (or the last day of a month \
         that has no such day)",
        money(quote.base_and_addons_cents),
        long_date(quote.first_charge_on),
    );
    let changes = "(b) changes to that amount when active users or add-ons change, in the \
                   amounts shown to me in the app and by email";
    let debits = match context {
        ConsentContext::Signup => format!(
            "{monthly}; {changes}; and (c) if onboarding is not completed by {deadline}, one month \
             of the subscription paid in advance, plus applicable sales tax, with the next charge at \
             least 7 days after the notice email ({advance} on {charge_on} with the plan and dates \
             of today), as described above.",
            advance = money(quote.advance_cents),
            charge_on = long_date(advance_charge_on(quote)),
            deadline = long_date(quote.deadline_on),
        ),
        ConsentContext::Invite => format!("{monthly}; and {changes}."),
    };
    format!(
        "By checking this box I authorize Granite Manager, through its payment processor \
         Stripe, to electronically debit the bank account I add on this page for: {debits} \
         {ACH_AUTHORIZATION_TERMS}"
    )
}

/// Hex SHA-256 of `text`.
pub fn sha256_hex(text: &str) -> String {
    use std::fmt::Write;
    Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// `$1,234.50`.
pub fn money(cents: i64) -> String {
    let digits = (cents / 100).abs().to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    let sign = if cents < 0 { "-" } else { "" };
    format!("{sign}${grouped}.{:02}", (cents % 100).abs())
}

/// `January 23, 2027`.
pub fn long_date(day: NaiveDate) -> String {
    day.format("%B %-d, %Y").to_string()
}

/// The onboarding advance checkbox (Terms 11), a signup's only.
fn onboarding_advance_text(quote: &Quote) -> String {
    format!(
        "I understand that onboarding with Granite Manager is free. If it is not completed by \
         {deadline} (30 days after signup), Granite Manager will email my company's Managers a \
         notice at least 7 days before the charge. The next subscription charge at least 7 days \
         after that notice (with the dates of today, the charge on {charge_on}) then also \
         includes the following month in advance: one month of the subscription at the time of \
         the notice, before tax ({advance} with the plan and users I chose), plus applicable \
         sales tax. That month is then not charged again, so nothing extra is paid, only one \
         month earlier. An onboarding call booked in the app that ends before the charge date \
         in the notice still avoids it, and if I request cancellation before that charge, it \
         is not charged. The deadline moves back one day for each day Granite Manager has no \
         bookable slot within the next 5 business days, and to match when Granite Manager \
         cancels or reschedules my call. I can request one 14-day extension.",
        deadline = long_date(quote.deadline_on),
        charge_on = long_date(advance_charge_on(quote)),
        advance = money(quote.advance_cents),
    )
}

/// The checkboxes of a signup (04-legal.md sections 6.1 and 6.2).
///
/// In order: terms, auto_renewal, per_user_pricing, onboarding_advance, then
/// the payment ones, card_authorization and ach_authorization. `app_url` is
/// the main app's origin, for the document links.
pub fn render(facts: &ConsentFacts, app_url: &str) -> Vec<RenderedConsent> {
    render_for(ConsentContext::Signup, facts, app_url)
}

/// The checkboxes of `context`, in order (an invite has no onboarding_advance);
/// the payment ones last.
pub fn render_for(
    context: ConsentContext,
    facts: &ConsentFacts,
    app_url: &str,
) -> Vec<RenderedConsent> {
    let quote = &facts.quote;
    let company = &facts.company_name;
    let first_charge = long_date(quote.first_charge_on);
    let origin = app_url.trim_end_matches('/');
    let link = |title: &str, path: &str| DocumentLink {
        title: title.to_string(),
        url: format!("{origin}{path}"),
    };
    let terms = format!(
        "I am authorized to bind {company}. I agree to the Terms of Service and Subscription \
         Terms (version {TERMS_VERSION}), the Data Processing Addendum (version {DPA_VERSION}), \
         and the Privacy Policy (version {PRIVACY_VERSION})."
    );
    let auto_renewal = format!(
        "I understand that this subscription is not charged before {first_charge}. On \
         {first_charge}, and then every month on the same day as the first charge (or the last \
         day of a month that has no such day), {company} will be charged {base} per month plus \
         $30 for each active user above 10 on the charge date, plus applicable sales tax, using \
         the payment method I provide, until I cancel. With {users} users that is {monthly} per month. I can \
         cancel any time with Request cancellation in the app or by emailing \
         support@granite-manager.com. Cancelling before {first_charge} means no charge. I will \
         get a reminder email 7 days before the first charge.",
        base = money(quote.base_and_addons_cents),
        users = quote.users,
        monthly = money(quote.monthly_cents),
    );
    let per_user = "I understand that the base price covers up to 10 active users. Each active \
                    user above 10 adds $30 per month. Granite Manager counts active users \
                    automatically and prorates the charge when users are added or removed, and \
                    I agree to this."
        .to_string();
    let onboarding = onboarding_advance_text(quote);
    let item = |kind, version: &'static str, text: String, documents| RenderedConsent {
        kind,
        version,
        sha256: sha256_hex(&text),
        text,
        documents,
        payment_method_type: None,
        document_version: version,
        document_sha256: None,
    };
    let mut terms = item(
        ConsentKind::Terms,
        TERMS_VERSION,
        terms,
        vec![
            link("Terms of Service", &format!("/terms?v={TERMS_VERSION}")),
            link(
                "Privacy Policy",
                &format!("/privacy-policy?v={PRIVACY_VERSION}"),
            ),
            link("Data Processing Addendum", &format!("/dpa?v={DPA_VERSION}")),
        ],
    );
    terms.document_sha256 = Some(TERMS_SHA256);
    let mut rendered = vec![
        terms,
        item(
            ConsentKind::AutoRenewal,
            AUTO_RENEWAL_VERSION,
            auto_renewal,
            Vec::new(),
        ),
        item(
            ConsentKind::PerUserPricing,
            PER_USER_PRICING_VERSION,
            per_user,
            Vec::new(),
        ),
    ];
    if context == ConsentContext::Signup {
        rendered.push(item(
            ConsentKind::OnboardingAdvance,
            ONBOARDING_ADVANCE_VERSION,
            onboarding,
            Vec::new(),
        ));
    }
    let payment = |kind, version, text: String, method| RenderedConsent {
        payment_method_type: Some(method),
        ..item(kind, version, text, Vec::new())
    };
    rendered.push(payment(
        ConsentKind::CardAuthorization,
        CARD_AUTHORIZATION_VERSION,
        CARD_AUTHORIZATION_TEXT.to_string(),
        PaymentMethodType::Card,
    ));
    rendered.push(payment(
        ConsentKind::AchAuthorization,
        ACH_AUTHORIZATION_VERSION,
        ach_authorization_text(context, facts),
        PaymentMethodType::UsBankAccount,
    ));
    rendered
}

/// Every rendered checkbox that applies to `method` was accepted.
///
/// `method` is the actual SetupIntent's. Each needs its version and hash:
/// the base ones and the matching payment one; the other payment one is
/// ignored.
pub fn all_accepted(
    rendered: &[RenderedConsent],
    given: &[ConsentAcceptance],
    method: PaymentMethodType,
) -> bool {
    rendered
        .iter()
        .filter(|consent| consent.applies_to(method))
        .all(|consent| {
            given.iter().any(|accepted| {
                let same_text = accepted.text_sha256 == consent.sha256;
                accepted.kind == consent.kind && accepted.version == consent.version && same_text
            })
        })
}

/// What a consent record says about the payment method. Never an account
/// or routing number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaymentFacts {
    pub stripe_customer_id: Option<String>,
    pub payment_method_id: Option<String>,
    pub method_type: Option<PaymentMethodType>,
    pub last4: Option<String>,
    /// The bank's name; bank accounts only.
    pub bank_name: Option<String>,
    /// Stripe's mandate for the debits; bank accounts only.
    pub mandate_id: Option<String>,
    /// `CCD` (a company account) or `WEB` (a personal one); bank accounts
    /// only, none when Stripe does not say.
    pub sec_code: Option<&'static str>,
}

fn stamp(at: NaiveDateTime) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Whose acceptance the records are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentOrigin<'a> {
    /// A self-serve signup (source `self_serve`); company and user come later.
    Signup { signup_id: &'a str },
    /// A plan invite accepted by a Manager (source `staff_move`).
    Invite {
        invite_id: &'a str,
        company_id: i32,
        /// The accepting Manager.
        user_id: i32,
        /// The superuser who made the invite.
        invited_by: i32,
        /// A legacy Stripe subscription ends where the new plan starts.
        legacy_subscription_replaced: bool,
    },
}

/// The `amounts_and_dates` JSON, the same on every record of an acceptance.
fn amounts_and_dates(
    origin: ConsentOrigin<'_>,
    facts: &ConsentFacts,
    payment: &PaymentFacts,
) -> serde_json::Value {
    let mut amounts = base_amounts(facts, payment);
    let extra = match origin {
        ConsentOrigin::Signup { .. } => {
            let quote = &facts.quote;
            json!({
                "onboarding_advance_cents": quote.advance_cents,
                "onboarding_deadline_on": quote.deadline_on,
                "onboarding_deadline_at": facts.deadline_at.map(stamp),
                "onboarding_notice_days": ONBOARDING_ADVANCE_NOTICE_DAYS,
                "onboarding_advance_charge_on": advance_charge_on(quote),
            })
        }
        ConsentOrigin::Invite {
            invite_id,
            invited_by,
            legacy_subscription_replaced,
            ..
        } => json!({
            "plan_invite_id": invite_id,
            "invited_by": invited_by,
            "legacy_subscription_replaced": legacy_subscription_replaced,
        }),
    };
    if let (Some(target), serde_json::Value::Object(extra)) = (amounts.as_object_mut(), extra) {
        target.extend(extra);
    }
    amounts
}

/// The fields every acceptance records.
fn base_amounts(facts: &ConsentFacts, payment: &PaymentFacts) -> serde_json::Value {
    let quote = &facts.quote;
    let line_cents = |kind: QuoteLineKind| {
        quote
            .lines
            .iter()
            .find(|line| line.kind == kind)
            .map(|line| line.unit_cents)
    };
    let addons: Vec<_> = quote
        .lines
        .iter()
        .filter(|line| line.kind == QuoteLineKind::Addon)
        .filter_map(|line| line.key)
        .collect();
    json!({
        "monthly_cents": quote.monthly_cents,
        "base_cents": line_cents(QuoteLineKind::Base),
        "base_and_addons_cents": quote.base_and_addons_cents,
        "extra_users": quote.extra_users,
        "extra_user_cents": facts.extra_user_cents,
        "users_estimate": quote.users,
        "addons": addons,
        "tax": "plus applicable sales tax",
        "first_charge_on": quote.first_charge_on,
        "trial_end_at": stamp(facts.trial_end_at),
        "grace_period_days": 7,
        "payment_method_type": payment.method_type,
        "payment_method_last4": payment.last4,
        "payment_method_bank_name": payment.bank_name,
        "documents": {
            (TERMS_VERSION): TERMS_SHA256,
            (PRIVACY_VERSION): PRIVACY_SHA256,
            (DPA_VERSION): DPA_SHA256,
        },
    })
}

/// Records the accepted consents, inside the caller's transaction.
///
/// One `consent_records` row per rendered checkbox and document it accepts
/// (the terms checkbox: terms, privacy and DPA), the payment checkbox of
/// `payment.method_type` only; that row also gets the mandate and SEC
/// code. A signup's: source
/// `self_serve`, `signup_intent_id` set, company and user still null. An
/// invite's: source `staff_move`, `plan_invite_id`, company and the accepting
/// Manager set.
pub async fn insert_records(
    conn: &mut MySqlConnection,
    origin: ConsentOrigin<'_>,
    rendered: &[RenderedConsent],
    facts: &ConsentFacts,
    payment: &PaymentFacts,
    meta: &RequestMeta,
    accepted_at: NaiveDateTime,
) -> sqlx::Result<()> {
    let amounts = amounts_and_dates(origin, facts, payment).to_string();
    let (source, signup_id, invite_id, company_id, user_id) = match origin {
        ConsentOrigin::Signup { signup_id } => ("self_serve", Some(signup_id), None, None, None),
        ConsentOrigin::Invite {
            invite_id,
            company_id,
            user_id,
            ..
        } => (
            "staff_move",
            None,
            Some(invite_id),
            Some(company_id),
            Some(user_id),
        ),
    };
    let rows: Vec<(&RenderedConsent, (&str, Option<&str>))> = rendered
        .iter()
        .filter(|consent| {
            consent.payment_method_type.is_none()
                || consent.payment_method_type == payment.method_type
        })
        .flat_map(|consent| {
            recorded_documents(consent)
                .into_iter()
                .map(move |document| (consent, document))
        })
        .collect();
    for (consent, (document_version, document_sha256)) in rows {
        let (mandate_id, sec_code) = if consent.payment_method_type.is_some() {
            (payment.mandate_id.as_deref(), payment.sec_code)
        } else {
            (None, None)
        };
        sqlx::query(
            "INSERT INTO consent_records
               (signup_intent_id, plan_invite_id, company_id, user_id, source, kind,
                document_version, document_sha256, checkbox_text, checkbox_sha256,
                amounts_and_dates, stripe_customer_id, payment_method_id, stripe_mandate_id,
                sec_code_intended, ip, user_agent, locale, accepted_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(signup_id)
        .bind(invite_id)
        .bind(company_id)
        .bind(user_id)
        .bind(source)
        .bind(consent.kind.as_str())
        .bind(document_version)
        .bind(document_sha256)
        .bind(&consent.text)
        .bind(&consent.sha256)
        .bind(&amounts)
        .bind(&payment.stripe_customer_id)
        .bind(&payment.payment_method_id)
        .bind(mandate_id)
        .bind(sec_code)
        .bind(&meta.ip)
        .bind(&meta.user_agent)
        .bind(&meta.locale)
        .bind(accepted_at)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::catalog::{QuoteLine, QuoteLineKind};

    fn facts(monthly: i64, base: i64) -> ConsentFacts {
        let day = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        ConsentFacts {
            company_name: "Acme Stone".into(),
            quote: Quote {
                users: 12,
                included_users: 10,
                extra_users: 2,
                lines: vec![QuoteLine {
                    kind: QuoteLineKind::Base,
                    key: None,
                    label: "Base plan (up to 10 users)".into(),
                    quantity: 1,
                    unit_cents: base,
                    amount_cents: base,
                }],
                base_and_addons_cents: base,
                monthly_cents: monthly,
                due_today_cents: 0,
                first_charge_on: day(2027, 1, 23),
                advance_cents: monthly,
                deadline_on: day(2026, 11, 22),
            },
            trial_end_at: day(2027, 1, 23).and_hms_opt(5, 0, 0).unwrap(),
            deadline_at: day(2026, 11, 22).and_hms_opt(17, 0, 0),
            extra_user_cents: 3_000,
        }
    }

    fn text_of(consents: &[RenderedConsent], kind: ConsentKind) -> &RenderedConsent {
        consents.iter().find(|c| c.kind == kind).unwrap()
    }

    #[test]
    fn money_has_cents_and_separators() {
        assert_eq!(money(37_500), "$375.00");
        assert_eq!(money(123_450), "$1,234.50");
        assert_eq!(money(5), "$0.05");
        assert_eq!(money(100_000_000), "$1,000,000.00");
    }

    #[test]
    fn dates_are_long() {
        let day = NaiveDate::from_ymd_opt(2027, 1, 3).unwrap();
        assert_eq!(long_date(day), "January 3, 2027");
    }

    #[test]
    fn the_signup_texts_are_exact() {
        let rendered = render(&facts(36_000, 30_000), "https://app.example/");
        let kinds: Vec<_> = rendered
            .iter()
            .map(|c| (c.kind, c.payment_method_type))
            .collect();
        assert_eq!(
            kinds,
            [
                (ConsentKind::Terms, None),
                (ConsentKind::AutoRenewal, None),
                (ConsentKind::PerUserPricing, None),
                (ConsentKind::OnboardingAdvance, None),
                (
                    ConsentKind::CardAuthorization,
                    Some(PaymentMethodType::Card)
                ),
                (
                    ConsentKind::AchAuthorization,
                    Some(PaymentMethodType::UsBankAccount)
                ),
            ]
        );
        assert_eq!(
            text_of(&rendered, ConsentKind::Terms).text,
            "I am authorized to bind Acme Stone. I agree to the Terms of Service and Subscription Terms (version terms-2026-10-v3), the Data Processing Addendum (version dpa-2026-10-v1), and the Privacy Policy (version privacy-2026-10-v2)."
        );
        assert_eq!(
            text_of(&rendered, ConsentKind::AutoRenewal).text,
            "I understand that this subscription is not charged before January 23, 2027. On January 23, 2027, and then every month on the same day as the first charge (or the last day of a month that has no such day), Acme Stone will be charged $300.00 per month plus $30 for each active user above 10 on the charge date, plus applicable sales tax, using the payment method I provide, until I cancel. With 12 users that is $360.00 per month. I can cancel any time with Request cancellation in the app or by emailing support@granite-manager.com. Cancelling before January 23, 2027 means no charge. I will get a reminder email 7 days before the first charge."
        );
        assert_eq!(
            text_of(&rendered, ConsentKind::PerUserPricing).text,
            "I understand that the base price covers up to 10 active users. Each active user above 10 adds $30 per month. Granite Manager counts active users automatically and prorates the charge when users are added or removed, and I agree to this."
        );
        assert_eq!(
            text_of(&rendered, ConsentKind::OnboardingAdvance).text,
            "I understand that onboarding with Granite Manager is free. If it is not completed by November 22, 2026 (30 days after signup), Granite Manager will email my company's Managers a notice at least 7 days before the charge. The next subscription charge at least 7 days after that notice (with the dates of today, the charge on January 23, 2027) then also includes the following month in advance: one month of the subscription at the time of the notice, before tax ($360.00 with the plan and users I chose), plus applicable sales tax. That month is then not charged again, so nothing extra is paid, only one month earlier. An onboarding call booked in the app that ends before the charge date in the notice still avoids it, and if I request cancellation before that charge, it is not charged. The deadline moves back one day for each day Granite Manager has no bookable slot within the next 5 business days, and to match when Granite Manager cancels or reschedules my call. I can request one 14-day extension."
        );
        let card = text_of(&rendered, ConsentKind::CardAuthorization);
        assert_eq!(card.version, "card-authorization-2026-10-v1");
        assert_eq!(
            card.text,
            "I authorize Granite Manager and Stripe to charge this card for the amounts and dates in the checkboxes above and to save it for future charges until I cancel."
        );
        let ach = text_of(&rendered, ConsentKind::AchAuthorization);
        assert_eq!(ach.version, "ach-authorization-2026-10-v2");
        assert_eq!(ach.document_version, ach.version);
        assert_eq!(
            ach.text,
            "By checking this box I authorize Granite Manager, through its payment processor Stripe, to electronically debit the bank account I add on this page for: (a) $300.00 per month plus $30 for each active user above 10, plus applicable sales tax, on January 23, 2027 and then every month on the same day as the first charge (or the last day of a month that has no such day); (b) changes to that amount when active users or add-ons change, in the amounts shown to me in the app and by email; and (c) if onboarding is not completed by November 22, 2026, one month of the subscription paid in advance, plus applicable sales tax, with the next charge at least 7 days after the notice email ($360.00 on January 23, 2027 with the plan and dates of today), as described above. If a debit is returned for insufficient funds, I authorize Granite Manager to retry it up to 2 times within 40 days, as Nacha rules allow. This authorization stays in effect until I revoke it by replacing the bank account on the account page or by emailing support@granite-manager.com, at least 3 business days before the next debit. If this is a business account, I agree to be bound by the Nacha Operating Rules for these debits. I confirm that I am authorized to debit this account. I can ask for a copy of this authorization at support@granite-manager.com."
        );
        assert_no_fee_or_dash(&rendered);
    }

    /// No customer text speaks of an onboarding fee (onboarding is free) or
    /// has an em or en dash.
    fn assert_no_fee_or_dash(rendered: &[RenderedConsent]) {
        for consent in rendered {
            let text = consent.text.to_lowercase();
            assert!(!text.contains("fee"), "{text}");
            assert!(
                !text.contains('\u{2014}') && !text.contains('\u{2013}'),
                "{text}"
            );
        }
    }

    #[test]
    fn every_checkbox_version_pins_its_wording() {
        let signup = render(&facts(36_000, 30_000), "https://app.example");
        let invite = render_for(
            ConsentContext::Invite,
            &invite_facts(),
            "https://app.example",
        );
        let pin = |rendered: &[RenderedConsent], kind| {
            let consent = text_of(rendered, kind);
            (consent.version, consent.sha256.clone())
        };
        let pins = [
            (TERMS_VERSION, TERMS_PIN),
            (AUTO_RENEWAL_VERSION, AUTO_RENEWAL_PIN),
            (PER_USER_PRICING_VERSION, PER_USER_PRICING_PIN),
            (ONBOARDING_ADVANCE_VERSION, ONBOARDING_ADVANCE_PIN),
            (CARD_AUTHORIZATION_VERSION, CARD_AUTHORIZATION_PIN),
            (ACH_AUTHORIZATION_VERSION, ACH_AUTHORIZATION_PINS[0]),
            (ACH_AUTHORIZATION_VERSION, ACH_AUTHORIZATION_PINS[1]),
        ]
        .map(|(version, sha)| (version, sha.to_string()));
        assert_eq!(
            [
                pin(&signup, ConsentKind::Terms),
                pin(&signup, ConsentKind::AutoRenewal),
                pin(&signup, ConsentKind::PerUserPricing),
                pin(&signup, ConsentKind::OnboardingAdvance),
                pin(&signup, ConsentKind::CardAuthorization),
                pin(&signup, ConsentKind::AchAuthorization),
                pin(&invite, ConsentKind::AchAuthorization),
            ],
            pins,
            "a checkbox text changed: give it a new version, then pin the new hash"
        );
    }

    #[test]
    fn hashes_and_documents() {
        let rendered = render(&facts(36_000, 30_000), "https://app.example/");
        for consent in &rendered {
            assert_eq!(consent.sha256, sha256_hex(&consent.text));
            assert_eq!(consent.sha256.len(), 64);
        }
        let terms = text_of(&rendered, ConsentKind::Terms);
        assert_eq!(terms.document_version, TERMS_VERSION);
        assert_eq!(terms.document_sha256, Some(TERMS_SHA256));
        assert_eq!(
            terms
                .documents
                .iter()
                .map(|d| d.url.as_str())
                .collect::<Vec<_>>(),
            [
                "https://app.example/terms?v=terms-2026-10-v3",
                "https://app.example/privacy-policy?v=privacy-2026-10-v2",
                "https://app.example/dpa?v=dpa-2026-10-v1"
            ]
        );
        let auto = text_of(&rendered, ConsentKind::AutoRenewal);
        assert_eq!(auto.version, "auto-renewal-2026-10-v2");
        assert_eq!(auto.document_version, auto.version);
        assert_eq!(auto.document_sha256, None);
        assert!(auto.documents.is_empty());
    }

    #[test]
    fn a_changed_price_changes_the_hash() {
        let before = render(&facts(36_000, 30_000), "https://app.example");
        let after = render(&facts(37_500, 31_500), "https://app.example");
        for kind in [
            ConsentKind::AutoRenewal,
            ConsentKind::OnboardingAdvance,
            ConsentKind::AchAuthorization,
        ] {
            assert_ne!(text_of(&before, kind).sha256, text_of(&after, kind).sha256);
        }
        for kind in [
            ConsentKind::Terms,
            ConsentKind::PerUserPricing,
            ConsentKind::CardAuthorization,
        ] {
            assert_eq!(text_of(&before, kind).sha256, text_of(&after, kind).sha256);
        }
    }

    fn accept(consent: &RenderedConsent) -> ConsentAcceptance {
        ConsentAcceptance {
            kind: consent.kind,
            version: consent.version.to_string(),
            text_sha256: consent.sha256.clone(),
        }
    }

    #[test]
    fn acceptance_needs_every_kind_with_the_same_version_and_hash() {
        let rendered = render(&facts(36_000, 30_000), "https://app.example");
        let card = PaymentMethodType::Card;
        let given: Vec<_> = rendered.iter().map(accept).collect();
        assert!(all_accepted(&rendered, &given, card));
        let mut extra = given.clone();
        extra.push(ConsentAcceptance {
            kind: ConsentKind::OnboardingAdvance,
            version: "x".into(),
            text_sha256: "y".into(),
        });
        assert!(all_accepted(&rendered, &extra, card));
        assert!(!all_accepted(&rendered, &given[..3], card));
        let mut stale = given.clone();
        stale[1].text_sha256 = "0".repeat(64);
        assert!(!all_accepted(&rendered, &stale, card));
        let mut old = given;
        old[2].version = "per-user-pricing-2026-09-v1".into();
        assert!(!all_accepted(&rendered, &old, card));
    }

    #[test]
    fn acceptance_needs_the_payment_checkbox_of_the_method_used() {
        let rendered = render(&facts(36_000, 30_000), "https://app.example");
        let with = |kind: Option<ConsentKind>| -> Vec<ConsentAcceptance> {
            rendered
                .iter()
                .filter(|c| c.payment_method_type.is_none() || Some(c.kind) == kind)
                .map(accept)
                .collect()
        };
        let (card, bank) = (PaymentMethodType::Card, PaymentMethodType::UsBankAccount);
        let card_only = with(Some(ConsentKind::CardAuthorization));
        let ach_only = with(Some(ConsentKind::AchAuthorization));
        assert!(all_accepted(&rendered, &card_only, card));
        assert!(!all_accepted(&rendered, &card_only, bank));
        assert!(all_accepted(&rendered, &ach_only, bank));
        assert!(!all_accepted(&rendered, &ach_only, card));
        assert!(!all_accepted(&rendered, &with(None), card));
        assert!(!all_accepted(&rendered, &with(None), bank));
    }

    /// An invite for 12 active users with Quotes, first charge Nov 23, 2026.
    fn invite_facts() -> ConsentFacts {
        let mut facts = facts(43_500, 37_500);
        facts.quote.first_charge_on = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        facts.trial_end_at = NaiveDate::from_ymd_opt(2026, 11, 23)
            .unwrap()
            .and_hms_opt(14, 0, 0)
            .unwrap();
        facts.deadline_at = None;
        facts
    }

    #[test]
    fn an_invite_renders_no_onboarding_advance() {
        let rendered = render_for(
            ConsentContext::Invite,
            &invite_facts(),
            "https://app.example",
        );
        let kinds: Vec<_> = rendered.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                ConsentKind::Terms,
                ConsentKind::AutoRenewal,
                ConsentKind::PerUserPricing,
                ConsentKind::CardAuthorization,
                ConsentKind::AchAuthorization,
            ]
        );
        assert_eq!(
            text_of(&rendered, ConsentKind::AchAuthorization).text,
            "By checking this box I authorize Granite Manager, through its payment processor Stripe, to electronically debit the bank account I add on this page for: (a) $375.00 per month plus $30 for each active user above 10, plus applicable sales tax, on November 23, 2026 and then every month on the same day as the first charge (or the last day of a month that has no such day); and (b) changes to that amount when active users or add-ons change, in the amounts shown to me in the app and by email. If a debit is returned for insufficient funds, I authorize Granite Manager to retry it up to 2 times within 40 days, as Nacha rules allow. This authorization stays in effect until I revoke it by replacing the bank account on the account page or by emailing support@granite-manager.com, at least 3 business days before the next debit. If this is a business account, I agree to be bound by the Nacha Operating Rules for these debits. I confirm that I am authorized to debit this account. I can ask for a copy of this authorization at support@granite-manager.com."
        );
        assert_eq!(
            text_of(&rendered, ConsentKind::AutoRenewal).text,
            "I understand that this subscription is not charged before November 23, 2026. On November 23, 2026, and then every month on the same day as the first charge (or the last day of a month that has no such day), Acme Stone will be charged $375.00 per month plus $30 for each active user above 10 on the charge date, plus applicable sales tax, using the payment method I provide, until I cancel. With 12 users that is $435.00 per month. I can cancel any time with Request cancellation in the app or by emailing support@granite-manager.com. Cancelling before November 23, 2026 means no charge. I will get a reminder email 7 days before the first charge."
        );
        assert_no_fee_or_dash(&rendered);
        let signup = render(&invite_facts(), "https://app.example");
        for kind in [
            ConsentKind::Terms,
            ConsentKind::AutoRenewal,
            ConsentKind::PerUserPricing,
            ConsentKind::CardAuthorization,
        ] {
            let (invite, signup) = (text_of(&rendered, kind), text_of(&signup, kind));
            assert_eq!(
                (invite.version, &invite.sha256),
                (signup.version, &signup.sha256)
            );
        }
    }

    #[test]
    fn invite_records_name_the_invite_and_no_onboarding() {
        let origin = ConsentOrigin::Invite {
            invite_id: "inv-1",
            company_id: 7,
            user_id: 9,
            invited_by: 3,
            legacy_subscription_replaced: true,
        };
        let amounts = amounts_and_dates(origin, &invite_facts(), &PaymentFacts::default());
        assert_eq!(amounts["plan_invite_id"], "inv-1");
        assert_eq!(amounts["invited_by"], 3);
        assert_eq!(amounts["legacy_subscription_replaced"], true);
        assert_eq!(amounts["first_charge_on"], "2026-11-23");
        assert_eq!(amounts["monthly_cents"], 43_500);
        assert!(amounts.get("onboarding_advance_cents").is_none());
        let signup = amounts_and_dates(
            ConsentOrigin::Signup { signup_id: "s-1" },
            &facts(36_000, 30_000),
            &PaymentFacts::default(),
        );
        assert_eq!(signup["onboarding_advance_cents"], 36_000);
        assert_eq!(signup["onboarding_advance_charge_on"], "2027-01-23");
        assert_eq!(signup["onboarding_deadline_at"], "2026-11-22T17:00:00Z");
        assert!(signup.get("plan_invite_id").is_none());
    }

    #[test]
    fn records_name_the_bank_but_never_its_numbers() {
        let payment = PaymentFacts {
            method_type: Some(PaymentMethodType::UsBankAccount),
            last4: Some("6789".into()),
            bank_name: Some("STRIPE TEST BANK".into()),
            ..PaymentFacts::default()
        };
        let amounts = amounts_and_dates(
            ConsentOrigin::Signup { signup_id: "s-1" },
            &facts(36_000, 30_000),
            &payment,
        );
        assert_eq!(amounts["payment_method_type"], "us_bank_account");
        assert_eq!(amounts["payment_method_last4"], "6789");
        assert_eq!(amounts["payment_method_bank_name"], "STRIPE TEST BANK");
        let text = amounts.to_string();
        assert!(!text.contains("routing") && !text.contains("account_number"));
    }
}
