//! The emails of a cancellation request (Terms 9, 11.7 and 17.2).
//!
//! To the company's Managers: the confirmation, the withdrawal notice, and
//! the new end date when a paid advance moves the end. To staff: the notice
//! (email and Telegram).
//!
//! Pure: subjects, escaped HTML and text
//! parts. The reason goes to staff only.

use super::reminder_emails::{
    RenderedEmail, SUPPORT_EMAIL, company_or_default, escape_html, footer, format_local_date,
    format_local_time, greeting, html_button, html_footer, html_paragraph, wrap_html,
};
use crate::onboarding_call::commitment::NOTICE_DAYS;
use chrono::{Duration, NaiveDateTime};

pub const CONFIRMATION_SUBJECT: &str = "We received your cancellation request";
pub const WITHDRAWAL_SUBJECT: &str = "Your Granite Manager subscription continues";
/// `billing_reminder_emails.step` of the new end date email (one per
/// Manager and request).
pub const MOVED_STEP: &str = "cancellation_moved";
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
    /// Why the onboarding advance is not charged; `None` when none applies
    /// (when it was paid, `effective_at` is the end of the prepaid month).
    pub onboarding_advance: Option<OnboardingAdvance>,
    /// The payments app's account page.
    pub account_url: &'a str,
}

/// Why a cancellation request avoids the advance payment, and what keeping
/// the subscription means for it (Terms 11.7 and 11.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingAdvance {
    /// Requested before the onboarding deadline.
    BeforeDeadline(NaiveDateTime),
    /// Requested after the deadline, before the advance notice was sent.
    BeforeNotice,
    /// Requested after the advance notice, before the charge it named.
    BeforeChargeDate(NaiveDateTime),
}

impl OnboardingAdvance {
    fn sentence(self) -> String {
        match self {
            Self::BeforeDeadline(at) => format!(
                "You requested cancellation before your onboarding deadline of {}, so no advance payment is charged. If you keep your subscription, the onboarding rules apply again: if onboarding is not completed by the deadline, we email a notice at least {NOTICE_DAYS} days before any advance payment is charged.",
                format_local_date(at)
            ),
            Self::BeforeNotice => format!(
                "You requested cancellation before we sent the advance payment notice, so no advance payment is charged. If you keep your subscription, we first send that notice, at least {NOTICE_DAYS} days before any advance payment is charged."
            ),
            Self::BeforeChargeDate(at) => {
                let date = format_local_date(at);
                format!(
                    "You requested cancellation before the charge on {date} that was to include the advance payment, so the advance payment is not charged. If you keep your subscription by {}, the advance payment is charged with the charge on {date}. If you keep it later, we first send a new notice at least {NOTICE_DAYS} days before any charge.",
                    format_local_time(at - Duration::days(NOTICE_DAYS))
                )
            }
        }
    }
}

/// The advance payment once a request is withdrawn (Terms 11.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalAdvance {
    /// The deadline is ahead: completing onboarding by then avoids the
    /// advance payment.
    Deadline(NaiveDateTime),
    /// Withdrawn at least 7 days before the charge date of the notice sent:
    /// the advance payment is charged with it.
    ChargedOn(NaiveDateTime),
    /// Withdrawn after the deadline with no notice out, or on or after the
    /// notice's charge date: a notice comes first.
    NewNotice,
    /// The commitment could not be read.
    Unknown,
}

impl WithdrawalAdvance {
    fn sentence(self) -> String {
        match self {
            Self::Deadline(at) => format!(
                "Onboarding is free. If it is completed by your onboarding deadline of {}, no advance payment is charged.",
                format_local_date(at)
            ),
            Self::ChargedOn(at) => format!(
                "You kept your subscription at least {NOTICE_DAYS} days before the charge date in our advance payment notice, so the advance payment is charged with the charge on {}, as the notice said. An onboarding call that ends before {} still avoids it.",
                format_local_date(at),
                format_local_time(at)
            ),
            Self::NewNotice => format!(
                "Your onboarding deadline has passed, so the advance payment applies again. We first send a new notice, at least {NOTICE_DAYS} days before it is charged. An onboarding call that ends before the charge date in that notice still avoids it."
            ),
            Self::Unknown => format!(
                "If your onboarding deadline has passed and onboarding is not completed, the advance payment applies as if cancellation had not been requested, after a notice at least {NOTICE_DAYS} days before it is charged."
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
    /// What the advance payment does now; `None` when none is pending.
    pub onboarding_advance: Option<WithdrawalAdvance>,
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
    paragraphs.extend(input.onboarding_advance.map(OnboardingAdvance::sentence));
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
    paragraphs.extend(input.onboarding_advance.map(WithdrawalAdvance::sentence));
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

/// One Manager's copy of the new end date: a paid advance payment moved the
/// scheduled end to the end of the month it prepaid.
#[derive(Debug, Clone, Copy)]
pub struct MovedInput<'a> {
    pub company_name: &'a str,
    pub user_name: Option<&'a str>,
    /// The end the request had.
    pub previous_effective_at: NaiveDateTime,
    /// The end now: the end of the prepaid month.
    pub effective_at: NaiveDateTime,
    pub account_url: &'a str,
}

fn moved_paragraphs(input: &MovedInput<'_>, esc: fn(&str) -> String) -> Vec<String> {
    let company = esc(company_or_default(input.company_name));
    let ends = format_local_date(input.effective_at);
    vec![
        format!(
            "The cancellation of the Granite Manager subscription for {company} now takes effect on {ends}, not on {}.",
            format_local_date(input.previous_effective_at)
        ),
        format!(
            "You paid one month in advance with your advance payment, up to {ends}, so the subscription now runs until then. Everything keeps working as usual for your whole team until that day."
        ),
        format!(
            "That month is covered by the credit of your advance payment, and nothing is charged after {ends}."
        ),
        format!(
            "Changed your mind? Before {ends}, use Keep my subscription on your account page, or reply to this email or write to {SUPPORT_EMAIL}."
        ),
    ]
}

/// The new end date of a cancellation request, for one Manager.
pub fn render_moved(input: &MovedInput<'_>) -> RenderedEmail {
    let hello = greeting(input.user_name);
    let company = company_or_default(input.company_name);
    let mut parts = vec![html_paragraph(&escape_html(&hello))];
    parts.extend(
        moved_paragraphs(input, escape_html)
            .iter()
            .map(|p| html_paragraph(p)),
    );
    parts.push(html_button(input.account_url, KEEP_LABEL));
    parts.push(html_footer(&escape_html(company)));
    let mut text = vec![hello];
    text.extend(moved_paragraphs(input, str::to_string));
    text.push(format!("{KEEP_LABEL}: {}", input.account_url));
    text.push(footer(company));
    RenderedEmail {
        subject: format!(
            "Your Granite Manager subscription now ends on {}",
            format_local_date(input.effective_at)
        ),
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
            onboarding_advance: Some(OnboardingAdvance::BeforeDeadline(at("2026-11-24 04:59:59"))),
            account_url: ACCOUNT,
        }
    }

    /// No dash, and no onboarding fee: onboarding is free.
    fn no_dashes(email: &RenderedEmail) {
        let all = format!("{}\n{}\n{}", email.subject, email.html, email.text);
        assert!(!all.contains('\u{2014}') && !all.contains('\u{2013}'));
        assert!(!all.to_lowercase().contains("fee"), "{all}");
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
            "You requested cancellation before your onboarding deadline of November 23, 2026, so no advance payment is charged."
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
            "If you keep your subscription, the onboarding rules apply again: if onboarding is not completed by the deadline, we email a notice at least 7 days before any advance payment is charged."
        ));
    }

    #[test]
    fn a_request_after_the_deadline_before_the_notice_avoids_the_advance() {
        let email = render_confirmation(&ConfirmationInput {
            onboarding_advance: Some(OnboardingAdvance::BeforeNotice),
            ..confirmation(false)
        });
        assert!(email.text.contains(
            "You requested cancellation before we sent the advance payment notice, so no advance payment is charged. If you keep your subscription, we first send that notice, at least 7 days before any advance payment is charged."
        ));
        assert!(!email.text.contains("onboarding deadline"));
        no_dashes(&email);
    }

    #[test]
    fn the_advance_sentence_names_the_charge_after_the_notice_and_is_left_out_without_one() {
        let notice = render_confirmation(&ConfirmationInput {
            onboarding_advance: Some(OnboardingAdvance::BeforeChargeDate(at(
                "2026-12-01 15:00:00",
            ))),
            ..confirmation(false)
        });
        assert!(notice.text.contains(
            "You requested cancellation before the charge on December 1, 2026 that was to include the advance payment, so the advance payment is not charged."
        ));
        // The withdrawal keeps the charge only 7 days or more before it.
        assert!(notice.text.contains(
            "If you keep your subscription by Tue, Nov 24 at 10:00 AM EST, the advance payment is charged with the charge on December 1, 2026. If you keep it later, we first send a new notice at least 7 days before any charge."
        ));
        assert!(!notice.text.contains("onboarding deadline"));
        let none = render_confirmation(&ConfirmationInput {
            onboarding_advance: None,
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
            onboarding_advance: None,
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
    fn the_withdrawal_says_when_the_advance_is_charged_or_that_a_notice_comes_first() {
        let input = |advance| WithdrawalInput {
            company_name: "Rock & Co",
            user_name: None,
            withdrawn_by: "Dana",
            next_charge_at: None,
            onboarding_advance: Some(advance),
            account_url: ACCOUNT,
        };
        let cases = [
            (
                WithdrawalAdvance::ChargedOn(at("2026-12-01 15:00:00")),
                "You kept your subscription at least 7 days before the charge date in our advance payment notice, so the advance payment is charged with the charge on December 1, 2026, as the notice said. An onboarding call that ends before Tue, Dec 1 at 10:00 AM EST still avoids it.",
            ),
            (
                WithdrawalAdvance::NewNotice,
                "Your onboarding deadline has passed, so the advance payment applies again. We first send a new notice, at least 7 days before it is charged. An onboarding call that ends before the charge date in that notice still avoids it.",
            ),
            (
                WithdrawalAdvance::Deadline(at("2026-11-24 04:59:59")),
                "Onboarding is free. If it is completed by your onboarding deadline of November 23, 2026, no advance payment is charged.",
            ),
            (
                WithdrawalAdvance::Unknown,
                "If your onboarding deadline has passed and onboarding is not completed, the advance payment applies as if cancellation had not been requested, after a notice at least 7 days before it is charged.",
            ),
        ];
        for (advance, sentence) in cases {
            let email = render_withdrawal(&input(advance));
            assert!(email.text.contains(sentence), "{advance:?}");
            assert!(email.html.contains(sentence), "{advance:?}");
            no_dashes(&email);
        }
    }

    #[test]
    fn the_moved_end_names_the_new_and_old_end_and_how_to_withdraw() {
        let email = render_moved(&MovedInput {
            company_name: "Rock & <Roll>",
            user_name: Some("Ann"),
            previous_effective_at: at("2027-01-21 05:00:00"),
            effective_at: at("2027-03-21 04:00:00"),
            account_url: ACCOUNT,
        });
        assert_eq!(
            email.subject,
            "Your Granite Manager subscription now ends on March 21, 2027"
        );
        assert_eq!(
            email.text,
            "Hi Ann,\n\n\
             The cancellation of the Granite Manager subscription for Rock & <Roll> now takes effect on March 21, 2027, not on January 21, 2027.\n\n\
             You paid one month in advance with your advance payment, up to March 21, 2027, so the subscription now runs until then. Everything keeps working as usual for your whole team until that day.\n\n\
             That month is covered by the credit of your advance payment, and nothing is charged after March 21, 2027.\n\n\
             Changed your mind? Before March 21, 2027, use Keep my subscription on your account page, or reply to this email or write to support@granite-manager.com.\n\n\
             Open your account page: https://payments.example.com/account\n\n\
             Sign in with your usual account to open the billing page. This email was sent to managers of Rock & <Roll>."
        );
        assert!(email.html.contains("Rock &amp; &lt;Roll&gt;"));
        assert_eq!(email.html.matches("href=").count(), 1);
        no_dashes(&email);
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
