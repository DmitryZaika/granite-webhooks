//! The plan invite emails: the invite to each Manager, and "your plan is
//! active" to the Manager who accepted. Transactional, plain words, no
//! promotion; replies go to support.

use crate::billing::catalog::{Catalog, INCLUDED_USERS, PlanChoice};
use crate::entitlements::AddonKey;
use crate::signup::consent::{long_date, money};
use crate::state::AppState;
use crate::webhooks_client::{OutgoingEmail, SendFailure};
use chrono::NaiveDate;
use std::fmt::Write as _;

pub const INVITE_SUBJECT: &str = "Your Granite Manager plan is ready to review";
pub const ACCEPTED_SUBJECT: &str = "Your Granite Manager plan is active";
const SUPPORT_EMAIL: &str = "support@granite-manager.com";

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// What the plan costs, as both emails list it. Money in cents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub base_cents: i64,
    /// (label, monthly cents) per add-on.
    pub addons: Vec<(&'static str, i64)>,
    pub extra_user_cents: i64,
    /// Active users now.
    pub users: i64,
    /// The monthly total with them.
    pub monthly_cents: i64,
    pub first_charge_on: NaiveDate,
}

impl Summary {
    pub fn of(
        catalog: &Catalog,
        users: i64,
        addons: &[AddonKey],
        first_charge_on: NaiveDate,
    ) -> Self {
        let quote = catalog.quote(&PlanChoice::normalized(users, addons), first_charge_on);
        Self {
            base_cents: catalog.base.unit_cents,
            addons: AddonKey::ALL
                .into_iter()
                .filter(|key| addons.contains(key))
                .map(|key| (key.label(), catalog.addon(key).unit_cents))
                .collect(),
            extra_user_cents: catalog.extra_user.unit_cents,
            users,
            monthly_cents: quote.monthly_cents,
            first_charge_on,
        }
    }

    fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "Base plan: {} per month, up to {INCLUDED_USERS} users",
            money(self.base_cents)
        )];
        if self.addons.is_empty() {
            lines.push("Add-ons: none".into());
        }
        for (label, cents) in &self.addons {
            lines.push(format!("{label}: {} per month", money(*cents)));
        }
        lines.push(format!(
            "Active users above {INCLUDED_USERS}: {} each per month",
            money(self.extra_user_cents)
        ));
        lines.push(format!(
            "With your {} active users: {} per month, plus applicable sales tax",
            self.users,
            money(self.monthly_cents)
        ));
        lines.push(format!("First charge: {}", long_date(self.first_charge_on)));
        lines
    }

    fn html(&self) -> String {
        let items = self.lines().iter().fold(String::new(), |mut out, line| {
            let _ = writeln!(
                out,
                r#"    <li style="margin: 4px 0;">{}</li>"#,
                escape(line)
            );
            out
        });
        format!(
            r#"  <ul style="color: #1e293b; line-height: 1.6; padding-left: 20px;">
{items}  </ul>"#
        )
    }
}

/// "Hi Jane," from the user's name; "Hi there," without one.
fn greeting(name: &str) -> String {
    let first = name
        .split_whitespace()
        .next()
        .filter(|first| !first.contains('@'))
        .unwrap_or("there");
    format!("Hi {},", escape(first))
}

fn paragraph(text: &str) -> String {
    format!(r#"  <p style="color: #475569; line-height: 1.6;">{text}</p>"#)
}

fn questions() -> String {
    format!(
        r#"  <p style="color: #94a3b8; font-size: 13px; margin-top: 24px;">Questions? Reply to this email or write to <a href="mailto:{SUPPORT_EMAIL}" style="color: #2563eb;">{SUPPORT_EMAIL}</a>.</p>"#
    )
}

/// The invite to one Manager.
pub struct InviteEmail<'a> {
    /// The Manager's name.
    pub name: &'a str,
    pub company_name: &'a str,
    pub summary: &'a Summary,
    /// When the legacy subscription ends (the new plan starts then).
    pub legacy_ends_on: Option<NaiveDate>,
    /// The Manager's `/upgrade/<token>` link.
    pub link: &'a str,
    /// The last day the link works.
    pub expires_on: NaiveDate,
}

pub fn invite_html(email: &InviteEmail<'_>) -> String {
    let company = escape(email.company_name);
    let legacy = email.legacy_ends_on.map_or_else(String::new, |ends_on| {
        paragraph(&format!(
            "Your current plan ends on {}. The new plan starts then, so you are never billed twice.",
            long_date(ends_on)
        )) + "\n"
    });
    format!(
        r#"<div style="font-family: Arial, sans-serif; max-width: 520px; margin: 0 auto;">
  <h2 style="color: #1e293b;">{INVITE_SUBJECT}</h2>
{hello}
{intro}
{summary}
{legacy}{unchanged}
  <a href="{link}" style="display: inline-block; background-color: #2563eb; color: white; padding: 12px 24px; border-radius: 6px; text-decoration: none; font-weight: 600; margin: 16px 0;">Review and accept your plan</a>
{expiry}
{footer}
</div>"#,
        hello = paragraph(&greeting(email.name)),
        intro = paragraph(&format!(
            "Granite Manager has set up a plan for {company}. Review it and accept it to start the plan."
        )),
        summary = email.summary.html(),
        unchanged = paragraph("Nothing changes until you accept."),
        link = escape(email.link),
        expiry = paragraph(&format!(
            "This link works until {}. Only a Manager of {company} can accept it.",
            long_date(email.expires_on)
        )),
        footer = questions(),
    )
}

/// "Your plan is active", to the Manager who accepted.
pub fn accepted_html(name: &str, company_name: &str, summary: &Summary) -> String {
    format!(
        r#"<div style="font-family: Arial, sans-serif; max-width: 520px; margin: 0 auto;">
  <h2 style="color: #1e293b;">{ACCEPTED_SUBJECT}</h2>
{hello}
{active}
{summary}
{nothing}
{footer}
</div>"#,
        hello = paragraph(&greeting(name)),
        active = paragraph(&format!(
            "{} is now on the Granite Manager plan.",
            escape(company_name)
        )),
        summary = summary.html(),
        nothing = paragraph(&format!(
            "Nothing is charged before {}. We will email you a reminder 7 days before the first charge.",
            long_date(summary.first_charge_on)
        )),
        footer = questions(),
    )
}

/// Sends one email; the failure's kind (never the address) on error.
pub async fn send(
    state: &AppState,
    to: &str,
    subject: &str,
    body: String,
) -> Result<(), &'static str> {
    let email = OutgoingEmail {
        from_name: Some("Granite Manager".into()),
        from_address: crate::billing::from_address(),
        to: vec![to.to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.into(),
        body,
        signature: None,
        reply_to: vec![SUPPORT_EMAIL.into()],
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    match state.webhooks.send_email(&email).await {
        Ok(_) => Ok(()),
        Err(SendFailure::Rejected(_)) => Err("rejected"),
        Err(SendFailure::Failed(_)) => Err("failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn summary(addons: Vec<(&'static str, i64)>) -> Summary {
        Summary {
            base_cents: 30_000,
            addons,
            extra_user_cents: 3_000,
            users: 12,
            monthly_cents: 43_500,
            first_charge_on: day(2026, 11, 23),
        }
    }

    #[test]
    fn the_invite_has_the_link_the_amounts_and_no_em_dash() {
        let html = invite_html(&InviteEmail {
            name: "Jane Doe",
            company_name: "Rock & Roll Stone",
            summary: &summary(vec![("Quotes", 7_500)]),
            legacy_ends_on: None,
            link: "https://payments.granite-manager.com/upgrade/abc_DEF-123",
            expires_on: day(2026, 11, 7),
        });
        for expected in [
            "Hi Jane,",
            "Granite Manager has set up a plan for Rock &amp; Roll Stone. Review it and accept it to start the plan.",
            "Base plan: $300.00 per month, up to 10 users",
            "Quotes: $75.00 per month",
            "Active users above 10: $30.00 each per month",
            "With your 12 active users: $435.00 per month, plus applicable sales tax",
            "First charge: November 23, 2026",
            "Nothing changes until you accept.",
            r#"href="https://payments.granite-manager.com/upgrade/abc_DEF-123""#,
            "Review and accept your plan",
            "This link works until November 7, 2026. Only a Manager of Rock &amp; Roll Stone can accept it.",
            "Questions? Reply to this email or write to",
        ] {
            assert!(html.contains(expected), "missing: {expected}");
        }
        assert!(!html.contains("Add-ons: none"));
        assert!(!html.contains("Your current plan ends"));
        assert!(!html.contains('\u{2014}'));
    }

    #[test]
    fn a_legacy_invite_says_when_the_current_plan_ends() {
        let html = invite_html(&InviteEmail {
            name: "",
            company_name: "Acme Stone",
            summary: &summary(Vec::new()),
            legacy_ends_on: Some(day(2026, 11, 14)),
            link: "https://payments.granite-manager.com/upgrade/x",
            expires_on: day(2026, 11, 7),
        });
        assert!(html.contains("Hi there,"));
        assert!(html.contains("Add-ons: none"));
        assert!(html.contains(
            "Your current plan ends on November 14, 2026. The new plan starts then, so you are never billed twice."
        ));
        assert!(!html.contains('\u{2014}'));
    }

    #[test]
    fn the_accepted_email_has_the_amounts_and_the_first_charge() {
        let html = accepted_html(
            "Jane Doe",
            "Acme Stone",
            &summary(vec![("Quotes", 7_500), ("QuickBooks", 3_000)]),
        );
        for expected in [
            "Hi Jane,",
            "Acme Stone is now on the Granite Manager plan.",
            "Quotes: $75.00 per month",
            "QuickBooks: $30.00 per month",
            "With your 12 active users: $435.00 per month, plus applicable sales tax",
            "Nothing is charged before November 23, 2026. We will email you a reminder 7 days before the first charge.",
        ] {
            assert!(html.contains(expected), "missing: {expected}");
        }
        assert!(!html.contains('\u{2014}'));
    }
}
