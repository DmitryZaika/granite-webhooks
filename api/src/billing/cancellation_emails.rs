//! The emails of a cancellation request: the confirmation and the withdrawal
//! notice to the company's Managers (04-legal 6.6, Terms 9 and 17.2), and the
//! staff notice (email and Telegram). Pure: subjects, escaped HTML and text
//! parts. The reason goes to staff only.

use super::reminder_emails::{
    RenderedEmail, SUPPORT_EMAIL, company_or_default, escape_html, footer, format_local_date,
    format_local_time, greeting, html_button, html_footer, html_paragraph, wrap_html,
};
use crate::onboarding_call::commitment::NOTICE_DAYS;
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
    /// Why the onboarding fee is not charged; `None` when no fee applies.
    pub onboarding_fee: Option<OnboardingFee>,
    /// The payments app's account page.
    pub account_url: &'a str,
}

/// Why a cancellation request avoids the onboarding fee, and what keeping
/// the subscription means for it (Terms 11.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingFee {
    /// Requested before the onboarding deadline.
    BeforeDeadline(NaiveDateTime),
    /// Requested after the deadline, before the fee notice was sent.
    BeforeNotice,
    /// Requested after the fee notice, before the fee's charge date.
    BeforeChargeDate(NaiveDateTime),
}

impl OnboardingFee {
    fn sentence(self) -> String {
        match self {
            Self::BeforeDeadline(at) => format!(
                "You requested cancellation before your onboarding deadline of {}, so the onboarding fee is not charged. If you keep your subscription, the usual onboarding rules apply again, and any onboarding fee comes with a notice at least {NOTICE_DAYS} days before it is charged.",
                format_local_date(at)
            ),
            Self::BeforeNotice => format!(
                "You requested cancellation before we sent the onboarding fee notice, so the onboarding fee is not charged. If you keep your subscription, we first send that notice, at least {NOTICE_DAYS} days before any onboarding fee is charged."
            ),
            Self::BeforeChargeDate(at) => {
                let date = format_local_date(at);
                format!(
                    "You requested cancellation before the charge date of the onboarding fee, {date}, so the fee is not charged. If you keep your subscription before {}, the fee is charged on {date}. If you keep it later, we first send a new notice at least {NOTICE_DAYS} days before any charge.",
                    format_local_time(at)
                )
            }
        }
    }
}

/// The onboarding fee once a request is withdrawn (Terms 11.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalFee {
    /// The deadline is ahead: completing onboarding by then avoids the fee.
    Deadline(NaiveDateTime),
    /// Withdrawn before the charge date of the notice sent: the fee is
    /// charged on it.
    ChargedOn(NaiveDateTime),
    /// Withdrawn after the deadline with no notice out, or on or after the
    /// notice's charge date: a notice comes first.
    NewNotice,
    /// The commitment could not be read.
    Unknown,
}

impl WithdrawalFee {
    fn sentence(self) -> String {
        match self {
            Self::Deadline(at) => format!(
                "The onboarding fee is not charged if onboarding is completed by your onboarding deadline of {}.",
                format_local_date(at)
            ),
            Self::ChargedOn(at) => format!(
                "You kept your subscription before the charge date in our onboarding fee notice, so the onboarding fee is charged on {}.",
                format_local_date(at)
            ),
            Self::NewNotice => format!(
                "Your onboarding deadline has passed, so the onboarding fee applies again. We first send an onboarding fee notice, at least {NOTICE_DAYS} days before the fee is charged."
            ),
            Self::Unknown => format!(
                "If your plan has an onboarding commitment whose deadline has passed, the onboarding fee applies as if cancellation had not been requested, after a notice at least {NOTICE_DAYS} days before it is charged."
            ),
        }
    }
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
    /// What the onboarding fee does now; `None` when no fee is pending.
    pub onboarding_fee: Option<WithdrawalFee>,
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
    let asked = format_local_date(input.requested_at);
    let ends = format_local_date(input.effective_at);
    let charges = if input.free_period {
        "The request was made during the free period, so the subscription is not charged at all."
            .to_string()
    } else {
        format!("No further subscription charges are made after {ends}.")
    };
    let mut paragraphs = vec![
        format!(
            "{by} requested cancellation of the Granite Manager subscription for {company} on {asked}."
        ),
        format!(
            "The subscription ends on {ends}. Until then everything keeps working as usual for your whole team."
        ),
        charges,
    ];
    paragraphs.extend(input.onboarding_fee.map(OnboardingFee::sentence));
    paragraphs.extend([
        format!(
            "Changed your mind? Before {ends}, use Keep my subscription on your account page, or reply to this email or write to {SUPPORT_EMAIL}."
        ),
        format!(
            "Need a copy of your data? Ask {SUPPORT_EMAIL} within {EXPORT_DAYS} days after {ends} and we will send an export in a common format."
        ),
    ]);
    paragraphs
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
                format_local_date(at)
            )
        },
    ));
    paragraphs.extend(input.onboarding_fee.map(WithdrawalFee::sentence));
    paragraphs.push("Questions? Reply to this email.".to_string());
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
            (ends, format_local_date(self.effective_at)),
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
            onboarding_fee: Some(OnboardingFee::BeforeDeadline(at("2026-11-24 04:59:59"))),
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
            "You requested cancellation before your onboarding deadline of November 23, 2026, so the onboarding fee is not charged."
        ));
        assert!(email.text.contains(
            "Before January 21, 2027, use Keep my subscription on your account page, or reply to this email or write to support@granite-manager.com."
        ));
        assert!(email.html.contains("or reply to this email or write to"));
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
    fn keeping_the_subscription_before_the_deadline_brings_back_the_usual_rules() {
        let email = render_confirmation(&confirmation(false));
        assert!(email.text.contains(
            "If you keep your subscription, the usual onboarding rules apply again, and any onboarding fee comes with a notice at least 7 days before it is charged."
        ));
    }

    #[test]
    fn a_request_after_the_deadline_before_the_notice_avoids_the_fee() {
        let email = render_confirmation(&ConfirmationInput {
            onboarding_fee: Some(OnboardingFee::BeforeNotice),
            ..confirmation(false)
        });
        assert!(email.text.contains(
            "You requested cancellation before we sent the onboarding fee notice, so the onboarding fee is not charged. If you keep your subscription, we first send that notice, at least 7 days before any onboarding fee is charged."
        ));
        assert!(!email.text.contains("onboarding deadline"));
        no_dashes(&email);
    }

    #[test]
    fn the_fee_sentence_names_the_charge_date_after_the_notice_and_is_left_out_without_a_fee() {
        let notice = render_confirmation(&ConfirmationInput {
            onboarding_fee: Some(OnboardingFee::BeforeChargeDate(at("2026-12-01 15:00:00"))),
            ..confirmation(false)
        });
        assert!(notice.text.contains(
            "You requested cancellation before the charge date of the onboarding fee, December 1, 2026, so the fee is not charged."
        ));
        assert!(notice.text.contains(
            "If you keep your subscription before Tue, Dec 1 at 10:00 AM EST, the fee is charged on December 1, 2026. If you keep it later, we first send a new notice at least 7 days before any charge."
        ));
        assert!(!notice.text.contains("onboarding deadline"));
        let none = render_confirmation(&ConfirmationInput {
            onboarding_fee: None,
            ..confirmation(false)
        });
        assert!(!none.text.contains("onboarding"));
        no_dashes(&notice);
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
            onboarding_fee: None,
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
        assert!(email.text.contains("Questions? Reply to this email."));
        assert!(email.html.contains("Questions? Reply to this email."));
        assert!(email.html.contains("Rock &amp; Co"));
        assert!(!email.text.contains("onboarding"));
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

    #[test]
    fn the_withdrawal_says_when_the_onboarding_fee_is_charged_or_that_a_notice_comes_first() {
        let input = |fee| WithdrawalInput {
            company_name: "Rock & Co",
            user_name: None,
            withdrawn_by: "Dana",
            next_charge_at: None,
            onboarding_fee: Some(fee),
            account_url: ACCOUNT,
        };
        let cases = [
            (
                WithdrawalFee::ChargedOn(at("2026-12-01 15:00:00")),
                "You kept your subscription before the charge date in our onboarding fee notice, so the onboarding fee is charged on December 1, 2026.",
            ),
            (
                WithdrawalFee::NewNotice,
                "Your onboarding deadline has passed, so the onboarding fee applies again. We first send an onboarding fee notice, at least 7 days before the fee is charged.",
            ),
            (
                WithdrawalFee::Deadline(at("2026-11-24 04:59:59")),
                "The onboarding fee is not charged if onboarding is completed by your onboarding deadline of November 23, 2026.",
            ),
            (
                WithdrawalFee::Unknown,
                "If your plan has an onboarding commitment whose deadline has passed, the onboarding fee applies as if cancellation had not been requested, after a notice at least 7 days before it is charged.",
            ),
        ];
        for (fee, sentence) in cases {
            let email = render_withdrawal(&input(fee));
            assert!(email.text.contains(sentence), "{fee:?}");
            assert!(email.html.contains(sentence), "{fee:?}");
            no_dashes(&email);
        }
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
