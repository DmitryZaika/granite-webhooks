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
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::MySqlConnection;
use utoipa::ToSchema;

/// Legal documents, copied from general_datebase
/// `app/content/legal/manifest.json` (a new version is a new pair here).
pub const TERMS_VERSION: &str = "terms-2026-10-v1";
pub const TERMS_SHA256: &str = "aecc6ea6791059bb5f402a9c6470466646febe88280ec326d01d812eec9678c3";
pub const PRIVACY_VERSION: &str = "privacy-2026-10-v2";
pub const PRIVACY_SHA256: &str = "a5c1a5ad7793ad455a709c474429fc1b5ab45829e2bfafd3249a5c90ecd9a2dc";
pub const DPA_VERSION: &str = "dpa-2026-10-v1";
pub const DPA_SHA256: &str = "a41370b6f1d617ea63ac1a01918d1bc38d88db13c5f2a62e71f7063c53184dc4";

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
    /// The onboarding fee if onboarding is not completed in 30 days.
    OnboardingFee,
    /// Reserved (step 13): ACH debit authorization.
    AchAuthorization,
    /// Reserved (step 13): card authorization.
    CardAuthorization,
}

impl ConsentKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Terms => "terms",
            Self::AutoRenewal => "auto_renewal",
            Self::PerUserPricing => "per_user_pricing",
            Self::OnboardingFee => "onboarding_fee",
            Self::AchAuthorization => "ach_authorization",
            Self::CardAuthorization => "card_authorization",
        }
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
    /// The page on the main app (`/terms`, `/privacy-policy`, `/dpa`).
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
    /// `consent_records.document_version`.
    #[serde(skip)]
    pub document_version: &'static str,
    /// `consent_records.document_sha256`, or none.
    #[serde(skip)]
    pub document_sha256: Option<&'static str>,
}

/// Which checkboxes a flow shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentContext {
    /// Self-serve signup: terms, auto_renewal, per_user_pricing, onboarding_fee.
    Signup,
    /// A company moved onto a plan by Granite Manager (plan invite): terms,
    /// auto_renewal, per_user_pricing. No onboarding commitment.
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

/// The onboarding fee is charged this long after its notice (at the deadline).
pub const ONBOARDING_FEE_NOTICE_DAYS: u64 = 7;

pub const AUTO_RENEWAL_VERSION: &str = "auto-renewal-2026-10-v1";
pub const PER_USER_PRICING_VERSION: &str = "per-user-pricing-2026-10-v1";
pub const ONBOARDING_FEE_VERSION: &str = "onboarding-fee-2026-10-v1";

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

/// The four checkboxes of 04-legal.md section 6.1, in order: terms,
/// auto_renewal, per_user_pricing, onboarding_fee. `app_url` is the main
/// app's origin, for the document links.
pub fn render(facts: &ConsentFacts, app_url: &str) -> Vec<RenderedConsent> {
    render_for(ConsentContext::Signup, facts, app_url)
}

/// The checkboxes of `context`, in order (an invite has no onboarding_fee).
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
         Terms (version {TERMS_VERSION}), the Data Processing Addendum, and the Privacy Policy \
         (version {PRIVACY_VERSION})."
    );
    let auto_renewal = format!(
        "I understand that my subscription starts with a free period. On {first_charge}, and \
         then every month, {company} will be charged {base} per month plus $30 for each active \
         user above 10 on the charge date, plus applicable sales tax, using the payment method \
         I provide, until I cancel. With {users} users that is {monthly} per month. I can \
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
    let onboarding = format!(
        "I understand that if onboarding with Granite Manager is not completed by {deadline} \
         (30 days after signup), a one-time onboarding fee of {fee} will be charged to my \
         payment method as a separate invoice, after a 7-day notice email. The fee is waived \
         if I complete onboarding by then or cancel before then. The deadline moves if Granite \
         Manager has no bookable slot, and I can request one 14-day extension.",
        deadline = long_date(quote.deadline_on),
        fee = money(quote.fee_cents),
    );
    let item = |kind, version: &'static str, text: String, documents| RenderedConsent {
        kind,
        version,
        sha256: sha256_hex(&text),
        text,
        documents,
        document_version: version,
        document_sha256: None,
    };
    let mut terms = item(
        ConsentKind::Terms,
        TERMS_VERSION,
        terms,
        vec![
            link("Terms of Service", "/terms"),
            link("Privacy Policy", "/privacy-policy"),
            link("Data Processing Addendum", "/dpa"),
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
            ConsentKind::OnboardingFee,
            ONBOARDING_FEE_VERSION,
            onboarding,
            Vec::new(),
        ));
    }
    rendered
}

/// Every rendered checkbox was accepted with its version and hash.
pub fn all_accepted(rendered: &[RenderedConsent], given: &[ConsentAcceptance]) -> bool {
    rendered.iter().all(|consent| {
        given.iter().any(|accepted| {
            let same_text = accepted.text_sha256 == consent.sha256;
            accepted.kind == consent.kind && accepted.version == consent.version && same_text
        })
    })
}

/// What a consent record says about the payment method.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaymentFacts {
    pub stripe_customer_id: Option<String>,
    pub payment_method_id: Option<String>,
    /// `card` or `us_bank_account`.
    pub method_type: Option<String>,
    pub last4: Option<String>,
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
                "onboarding_fee_cents": quote.fee_cents,
                "onboarding_deadline_on": quote.deadline_on,
                "onboarding_deadline_at": facts.deadline_at.map(stamp),
                "onboarding_fee_notice_days": ONBOARDING_FEE_NOTICE_DAYS,
                "onboarding_fee_charge_on": quote.deadline_on + chrono::Days::new(ONBOARDING_FEE_NOTICE_DAYS),
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
/// (the terms checkbox: terms, privacy and DPA). A signup's: source
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
        .flat_map(|consent| {
            recorded_documents(consent)
                .into_iter()
                .map(move |document| (consent, document))
        })
        .collect();
    for (consent, (document_version, document_sha256)) in rows {
        sqlx::query(
            "INSERT INTO consent_records
               (signup_intent_id, plan_invite_id, company_id, user_id, source, kind,
                document_version, document_sha256, checkbox_text, checkbox_sha256,
                amounts_and_dates, stripe_customer_id, payment_method_id, ip, user_agent,
                locale, accepted_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                fee_cents: 2 * base,
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
    fn the_four_texts_are_exact() {
        let rendered = render(&facts(36_000, 30_000), "https://app.example/");
        let kinds: Vec<_> = rendered.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                ConsentKind::Terms,
                ConsentKind::AutoRenewal,
                ConsentKind::PerUserPricing,
                ConsentKind::OnboardingFee
            ]
        );
        assert_eq!(
            text_of(&rendered, ConsentKind::Terms).text,
            "I am authorized to bind Acme Stone. I agree to the Terms of Service and Subscription Terms (version terms-2026-10-v1), the Data Processing Addendum, and the Privacy Policy (version privacy-2026-10-v2)."
        );
        assert_eq!(
            text_of(&rendered, ConsentKind::AutoRenewal).text,
            "I understand that my subscription starts with a free period. On January 23, 2027, and then every month, Acme Stone will be charged $300.00 per month plus $30 for each active user above 10 on the charge date, plus applicable sales tax, using the payment method I provide, until I cancel. With 12 users that is $360.00 per month. I can cancel any time with Request cancellation in the app or by emailing support@granite-manager.com. Cancelling before January 23, 2027 means no charge. I will get a reminder email 7 days before the first charge."
        );
        assert_eq!(
            text_of(&rendered, ConsentKind::PerUserPricing).text,
            "I understand that the base price covers up to 10 active users. Each active user above 10 adds $30 per month. Granite Manager counts active users automatically and prorates the charge when users are added or removed, and I agree to this."
        );
        assert_eq!(
            text_of(&rendered, ConsentKind::OnboardingFee).text,
            "I understand that if onboarding with Granite Manager is not completed by November 22, 2026 (30 days after signup), a one-time onboarding fee of $600.00 will be charged to my payment method as a separate invoice, after a 7-day notice email. The fee is waived if I complete onboarding by then or cancel before then. The deadline moves if Granite Manager has no bookable slot, and I can request one 14-day extension."
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
                "https://app.example/terms",
                "https://app.example/privacy-policy",
                "https://app.example/dpa"
            ]
        );
        let auto = text_of(&rendered, ConsentKind::AutoRenewal);
        assert_eq!(auto.version, "auto-renewal-2026-10-v1");
        assert_eq!(auto.document_version, auto.version);
        assert_eq!(auto.document_sha256, None);
        assert!(auto.documents.is_empty());
    }

    #[test]
    fn a_changed_price_changes_the_hash() {
        let before = render(&facts(36_000, 30_000), "https://app.example");
        let after = render(&facts(37_500, 31_500), "https://app.example");
        for kind in [ConsentKind::AutoRenewal, ConsentKind::OnboardingFee] {
            assert_ne!(text_of(&before, kind).sha256, text_of(&after, kind).sha256);
        }
        for kind in [ConsentKind::Terms, ConsentKind::PerUserPricing] {
            assert_eq!(text_of(&before, kind).sha256, text_of(&after, kind).sha256);
        }
    }

    #[test]
    fn acceptance_needs_every_kind_with_the_same_version_and_hash() {
        let rendered = render(&facts(36_000, 30_000), "https://app.example");
        let given: Vec<_> = rendered
            .iter()
            .map(|c| ConsentAcceptance {
                kind: c.kind,
                version: c.version.to_string(),
                text_sha256: c.sha256.clone(),
            })
            .collect();
        assert!(all_accepted(&rendered, &given));
        let mut extra = given.clone();
        extra.push(ConsentAcceptance {
            kind: ConsentKind::AchAuthorization,
            version: "x".into(),
            text_sha256: "y".into(),
        });
        assert!(all_accepted(&rendered, &extra));
        assert!(!all_accepted(&rendered, &given[..3]));
        let mut stale = given.clone();
        stale[1].text_sha256 = "0".repeat(64);
        assert!(!all_accepted(&rendered, &stale));
        let mut old = given;
        old[2].version = "per-user-pricing-2026-09-v1".into();
        assert!(!all_accepted(&rendered, &old));
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
    fn an_invite_renders_terms_auto_renewal_and_per_user_pricing_only() {
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
                ConsentKind::PerUserPricing
            ]
        );
        assert_eq!(
            text_of(&rendered, ConsentKind::AutoRenewal).text,
            "I understand that my subscription starts with a free period. On November 23, 2026, and then every month, Acme Stone will be charged $375.00 per month plus $30 for each active user above 10 on the charge date, plus applicable sales tax, using the payment method I provide, until I cancel. With 12 users that is $435.00 per month. I can cancel any time with Request cancellation in the app or by emailing support@granite-manager.com. Cancelling before November 23, 2026 means no charge. I will get a reminder email 7 days before the first charge."
        );
        let signup = render(&invite_facts(), "https://app.example");
        for kind in [
            ConsentKind::Terms,
            ConsentKind::AutoRenewal,
            ConsentKind::PerUserPricing,
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
        assert!(amounts.get("onboarding_fee_cents").is_none());
        let signup = amounts_and_dates(
            ConsentOrigin::Signup { signup_id: "s-1" },
            &facts(36_000, 30_000),
            &PaymentFacts::default(),
        );
        assert_eq!(signup["onboarding_fee_cents"], 60_000);
        assert_eq!(signup["onboarding_deadline_at"], "2026-11-22T17:00:00Z");
        assert!(signup.get("plan_invite_id").is_none());
    }
}
