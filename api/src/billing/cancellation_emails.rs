//! The emails of a cancellation request: the confirmation and the withdrawal
//! notice to the company's Managers (04-legal 6.6, Terms 9 and 17.2), and the
//! staff notice (email and Telegram). Pure: subjects, escaped HTML and text
//! parts. The reason goes to staff only.

use super::format_billing_date;
use super::reminder_emails::{
    RenderedEmail, SUPPORT_EMAIL, company_or_default, escape_html, footer, greeting, html_button,
    html_footer, html_paragraph, wrap_html,
};
use chrono::NaiveDateTime;

pub const CONFIRMATION_SUBJECT: &str = "We received your cancellation request";
pub const WITHDRAWAL_SUBJECT: &str = "Your Granite Manager subscription continues";
const KEEP_LABEL: &str = "Open your account page";
/// Days after the end date a data export can be asked for (Terms 17.2).
pub const EXPORT_DAYS: u32 = 30;

/// One Manager's copy of the confirmation.
#[derive(Debug, Clone, Copy)]
pub struct ConfirmationInput<'a> {
    pub company_name: &'a str,
    /// The recipient.
    pub user_name: Option<&'a str>,
    /// Who asked (a Manager's name).
    pub requested_by: &'a str,
    pub requested_at: NaiveDateTime,
    /// When the subscription ends.
    pub effective_at: NaiveDateTime,
    /// Asked during the free period: nothing is charged at all.
    pub free_period: bool,
    /// The payments app's account page.
    pub account_url: &'a str,
}

/// One Manager's copy of the withdrawal notice.
#[derive(Debug, Clone, Copy)]
pub struct WithdrawalInput<'a> {
    pub company_name: &'a str,
    pub user_name: Option<&'a str>,
    /// Who withdrew it.
    pub withdrawn_by: &'a str,
    /// The next charge, when known.
    pub next_charge_at: Option<NaiveDateTime>,
    pub account_url: &'a str,
}

fn requester_or_default(name: &str) -> &str {
    let name = name.trim();
    if name.is_empty() { "A Manager" } else { name }
}

/// Paragraphs of the confirmation; `esc` escapes user text for HTML (or not).
fn confirmation_paragraphs(input: &ConfirmationInput<'_>, esc: fn(&str) -> String) -> Vec<String> {
    let company = esc(company_or_default(input.company_name));
    let by = esc(requester_or_default(input.requested_by));
    let asked = format_billing_date(input.requested_at);
    let ends = format_billing_date(input.effective_at);
    let charges = if input.free_period {
        "The request was made during the free period, so the subscription is not charged at all."
            .to_string()
    } else {
        format!("No further subscription charges are made after {ends}.")
    };
    vec![
        format!(
            "{by} requested cancellation of the Granite Manager subscription for {company} on {asked}."
        ),
        format!(
            "The subscription ends on {ends}. Until then everything keeps working as usual for your whole team."
        ),
        charges,
        "If your plan has an onboarding commitment, the onboarding fee is not charged when cancellation is requested before the onboarding deadline.".to_string(),
        format!(
            "Changed your mind? Before {ends}, use Keep my subscription on your account page, or reply to this email or write to {SUPPORT_EMAIL}."
        ),
        format!(
            "Need a copy of your data? Ask {SUPPORT_EMAIL} within {EXPORT_DAYS} days after {ends} and we will send an export in a common format."
        ),
    ]
}

/// The confirmation of a cancellation request, for one Manager.
pub fn render_confirmation(input: &ConfirmationInput<'_>) -> RenderedEmail {
    let hello = greeting(input.user_name);
    let company = company_or_default(input.company_name);
    let mut parts = vec![html_paragraph(&escape_html(&hello))];
    parts.extend(
        confirmation_paragraphs(input, escape_html)
            .iter()
            .map(|p| html_paragraph(p)),
    );
    parts.push(html_button(input.account_url, KEEP_LABEL));
    parts.push(html_footer(&escape_html(company)));
    let mut text = vec![hello];
    text.extend(confirmation_paragraphs(input, str::to_string));
    text.push(format!("{KEEP_LABEL}: {}", input.account_url));
    text.push(footer(company));
    RenderedEmail {
        subject: CONFIRMATION_SUBJECT.to_string(),
        html: wrap_html(parts),
        text: text.join("\n\n"),
    }
}

fn withdrawal_paragraphs(input: &WithdrawalInput<'_>, esc: fn(&str) -> String) -> Vec<String> {
    let company = esc(company_or_default(input.company_name));
    let by = esc(requester_or_default(input.withdrawn_by));
    let mut paragraphs = vec![format!(
        "{by} withdrew the cancellation request for {company}. The Granite Manager subscription continues as before."
    )];
    paragraphs.push(input.next_charge_at.map_or_else(
        || "Charges continue every month as before.".to_string(),
        |at| {
            format!(
                "The next charge is on {}, and then every month on the same day.",
                format_billing_date(at)
            )
        },
    ));
    paragraphs.push(
        "If your plan has an onboarding commitment whose deadline has passed, the onboarding fee applies as if cancellation had not been requested.".to_string(),
    );
    paragraphs
}

/// The notice that a cancellation request was withdrawn, for one Manager.
pub fn render_withdrawal(input: &WithdrawalInput<'_>) -> RenderedEmail {
    let hello = greeting(input.user_name);
    let company = company_or_default(input.company_name);
    let mut parts = vec![html_paragraph(&escape_html(&hello))];
    parts.extend(
        withdrawal_paragraphs(input, escape_html)
            .iter()
            .map(|p| html_paragraph(p)),
    );
    parts.push(html_button(input.account_url, KEEP_LABEL));
    parts.push(html_footer(&escape_html(company)));
    let mut text = vec![hello];
    text.extend(withdrawal_paragraphs(input, str::to_string));
    text.push(format!("{KEEP_LABEL}: {}", input.account_url));
    text.push(footer(company));
    RenderedEmail {
        subject: WITHDRAWAL_SUBJECT.to_string(),
        html: wrap_html(parts),
        text: text.join("\n\n"),
    }
}

/// What staff hear about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaffEvent {
    Requested,
    Withdrawn,
}

#[derive(Debug, Clone, Copy)]
pub struct StaffNoticeInput<'a> {
    pub event: StaffEvent,
    pub company_name: &'a str,
    pub company_id: i32,
    /// Who requested (or withdrew) it.
    pub by: &'a str,
    pub effective_at: NaiveDateTime,
    pub reason: Option<&'a str>,
}

impl StaffNoticeInput<'_> {
    const fn title(&self) -> &'static str {
        match self.event {
            StaffEvent::Requested => "Cancellation requested",
            StaffEvent::Withdrawn => "Cancellation withdrawn",
        }
    }

    fn rows(&self) -> Vec<(&'static str, String)> {
        let by = match self.event {
            StaffEvent::Requested => "Requested by",
            StaffEvent::Withdrawn => "Withdrawn by",
        };
        let ends = match self.event {
            StaffEvent::Requested => "Ends on",
            StaffEvent::Withdrawn => "Was to end on",
        };
        vec![
            (
                "Company",
                format!(
                    "{} (id {})",
                    company_or_default(self.company_name),
                    self.company_id
                ),
            ),
            (by, requester_or_default(self.by).to_string()),
            (ends, format_billing_date(self.effective_at)),
            ("Reason", self.reason.unwrap_or("none given").to_string()),
        ]
    }
}

/// The staff email: company, requester, end date and the reason.
pub fn render_staff_email(input: &StaffNoticeInput<'_>) -> RenderedEmail {
    let rows = input.rows();
    let subject = format!(
        "{}: {}",
        input.title(),
        company_or_default(input.company_name)
    );
    let mut parts = vec![html_paragraph(&format!(
        "<strong>{}</strong>",
        input.title()
    ))];
    parts.extend(
        rows.iter()
            .map(|(label, value)| html_paragraph(&format!("{label}: {}", escape_html(value)))),
    );
    let mut text = vec![input.title().to_string()];
    text.extend(
        rows.iter()
            .map(|(label, value)| format!("{label}: {value}")),
    );
    RenderedEmail {
        subject,
        html: wrap_html(parts),
        text: text.join("\n"),
    }
}

/// The staff Telegram message: short, the reason cut to 200 characters.
pub fn render_staff_telegram(input: &StaffNoticeInput<'_>) -> String {
    let mut rows = input.rows();
    if let Some((_, reason)) = rows.last_mut()
        && reason.chars().count() > 200
    {
        *reason = format!("{}...", reason.chars().take(200).collect::<String>());
    }
    let mut lines = vec![input.title().to_string()];
    lines.extend(
        rows.iter()
            .map(|(label, value)| format!("{label}: {value}")),
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::test_store::at;

    const ACCOUNT: &str = "https://payments.example.com/account";

    fn confirmation(free_period: bool) -> ConfirmationInput<'static> {
        ConfirmationInput {
            company_name: "Rock & <Roll>",
            user_name: Some("Ann <b>"),
            requested_by: "Dana \"D\" Manager",
            requested_at: at("2026-10-24 15:00:00"),
            effective_at: at("2027-01-21 05:00:00"),
            free_period,
            account_url: ACCOUNT,
        }
    }

    fn no_dashes(email: &RenderedEmail) {
        let all = format!("{}\n{}\n{}", email.subject, email.html, email.text);
        assert!(!all.contains('\u{2014}') && !all.contains('\u{2013}'));
    }

    #[test]
    fn the_confirmation_states_who_when_the_end_and_how_to_withdraw() {
        let email = render_confirmation(&confirmation(false));
        assert_eq!(email.subject, "We received your cancellation request");
        assert!(email.text.starts_with("Hi Ann <b>,"));
        assert!(email.text.contains(
            "Dana \"D\" Manager requested cancellation of the Granite Manager subscription for Rock & <Roll> on October 24, 2026."
        ));
        assert!(email.text.contains(
            "The subscription ends on January 21, 2027. Until then everything keeps working as usual for your whole team."
        ));
        assert!(
            email
                .text
                .contains("No further subscription charges are made after January 21, 2027.")
        );
        assert!(email.text.contains(
            "the onboarding fee is not charged when cancellation is requested before the onboarding deadline."
        ));
        assert!(email.text.contains(
            "Before January 21, 2027, use Keep my subscription on your account page, or reply to this email or write to support@granite-manager.com."
        ));
        assert!(
            email
                .text
                .contains("Ask support@granite-manager.com within 30 days after January 21, 2027")
        );
        assert!(
            email
                .text
                .contains("Open your account page: https://payments.example.com/account")
        );
        assert!(!email.text.contains("not charged at all"));
        no_dashes(&email);
    }

    #[test]
    fn the_confirmation_escapes_names_in_html() {
        let email = render_confirmation(&confirmation(false));
        assert!(email.html.contains("Hi Ann &lt;b&gt;,"));
        assert!(email.html.contains("Dana &quot;D&quot; Manager requested"));
        assert!(email.html.contains("Rock &amp; &lt;Roll&gt;"));
        assert!(!email.html.contains("<b>") && !email.html.contains("<Roll>"));
        assert!(
            email
                .html
                .contains("href=\"https://payments.example.com/account\"")
        );
        assert_eq!(email.html.matches("href=").count(), 1);
    }

    #[test]
    fn a_request_in_the_free_period_is_never_charged() {
        let email = render_confirmation(&confirmation(true));
        assert!(email.text.contains(
            "The request was made during the free period, so the subscription is not charged at all."
        ));
        assert!(!email.text.contains("No further subscription charges"));
        no_dashes(&email);
    }

    #[test]
    fn the_confirmation_has_no_retention_pitch() {
        let email = render_confirmation(&confirmation(false))
            .text
            .to_lowercase();
        for word in ["discount", "offer", "upgrade", "sorry to see"] {
            assert!(!email.contains(word), "{word}");
        }
    }

    #[test]
    fn the_withdrawal_names_the_next_charge_when_known() {
        let input = WithdrawalInput {
            company_name: "Rock & Co",
            user_name: None,
            withdrawn_by: "Dana",
            next_charge_at: Some(at("2027-01-21 05:00:00")),
            account_url: ACCOUNT,
        };
        let email = render_withdrawal(&input);
        assert_eq!(email.subject, "Your Granite Manager subscription continues");
        assert!(email.text.starts_with("Hello,"));
        assert!(email.text.contains(
            "Dana withdrew the cancellation request for Rock & Co. The Granite Manager subscription continues as before."
        ));
        assert!(email.text.contains(
            "The next charge is on January 21, 2027, and then every month on the same day."
        ));
        assert!(email.html.contains("Rock &amp; Co"));
        no_dashes(&email);

        let unknown = render_withdrawal(&WithdrawalInput {
            next_charge_at: None,
            withdrawn_by: " ",
            ..input
        });
        assert!(
            unknown
                .text
                .contains("Charges continue every month as before.")
        );
        assert!(unknown.text.contains("A Manager withdrew"));
    }

    fn staff(reason: Option<&str>) -> StaffNoticeInput<'_> {
        StaffNoticeInput {
            event: StaffEvent::Requested,
            company_name: "Rock & <Roll>",
            company_id: 7,
            by: "Dana",
            effective_at: at("2027-01-21 05:00:00"),
            reason,
        }
    }

    #[test]
    fn staff_hear_the_company_requester_date_and_reason() {
        let email = render_staff_email(&staff(Some("Too <expensive>")));
        assert_eq!(email.subject, "Cancellation requested: Rock & <Roll>");
        assert!(email.text.contains("Company: Rock & <Roll> (id 7)"));
        assert!(email.text.contains("Requested by: Dana"));
        assert!(email.text.contains("Ends on: January 21, 2027"));
        assert!(email.text.contains("Reason: Too <expensive>"));
        assert!(email.html.contains("Reason: Too &lt;expensive&gt;"));
        assert!(email.html.contains("Rock &amp; &lt;Roll&gt; (id 7)"));
        no_dashes(&email);

        let telegram = render_staff_telegram(&staff(None));
        assert_eq!(
            telegram,
            "Cancellation requested\nCompany: Rock & <Roll> (id 7)\nRequested by: Dana\nEnds on: January 21, 2027\nReason: none given"
        );
        let long = "x".repeat(400);
        let telegram = render_staff_telegram(&staff(Some(&long)));
        assert!(telegram.ends_with(&format!("Reason: {}...", "x".repeat(200))));

        let withdrawn = render_staff_telegram(&StaffNoticeInput {
            event: StaffEvent::Withdrawn,
            ..staff(None)
        });
        assert!(withdrawn.starts_with("Cancellation withdrawn\n"));
        assert!(withdrawn.contains("Withdrawn by: Dana"));
        assert!(withdrawn.contains("Was to end on: January 21, 2027"));
    }
}
