//! The onboarding emails to a company's Managers: the reminders on days 7,
//! 21 and 27 and the fee notice (04-legal.md sections 6.5 and Terms 11).
//! Subjects, HTML (escaped) and plain-text parts, in the billing emails'
//! look. Each email has one link. Every date is the buyer's calendar day
//! and a call's time carries its zone (`SIGNUP_TZ`).

use super::commitment::{EXTENSION_DAYS, ReminderStep, SLOT_WINDOW_BUSINESS_DAYS};
use crate::billing::reminder_emails::{
    RenderedEmail, SUPPORT_EMAIL, company_or_default, escape_html, format_local_date,
    format_local_time, format_money, greeting, html_button, html_paragraph, wrap_html,
};
use chrono::NaiveDateTime;

/// Where the reminders' button goes: the booking step in the CRM.
pub const BOOKING_PATH: &str = "/employee?book_onboarding=1";
/// Where the notice's link goes (Section 11).
pub const TERMS_PATH: &str = "/terms";

fn link(app_url: &str, path: &str) -> String {
    format!("{}{path}", app_url.trim_end_matches('/'))
}

fn footer(company: &str) -> String {
    format!("This email was sent to managers of {company}.")
}

fn html_footer(company_html: &str) -> String {
    format!(
        "<p style=\"margin:0;font-size:13px;line-height:20px;color:#6b7280\">{}</p>",
        footer(company_html)
    )
}

/// Paragraphs written once and rendered twice: plain, and with every value
/// escaped for HTML.
fn render(
    subject: String,
    user_name: Option<&str>,
    company: &str,
    paragraphs: impl Fn(&dyn Fn(&str) -> String) -> Vec<String>,
    action: &str,
    url: &str,
) -> RenderedEmail {
    let hello = greeting(user_name);
    let plain = paragraphs(&|value: &str| value.to_string());
    let escaped = paragraphs(&escape_html);
    let company_html = escape_html(company);
    let mut parts = vec![html_paragraph(&escape_html(&hello))];
    parts.extend(escaped.iter().map(|paragraph| html_paragraph(paragraph)));
    parts.push(html_button(url, action));
    parts.push(html_footer(&company_html));
    let mut text = vec![hello];
    text.extend(plain);
    text.push(format!("{action}: {url}"));
    text.push(footer(company));
    RenderedEmail {
        subject,
        html: wrap_html(parts),
        text: text.join("\n\n"),
    }
}

/// A reminder on day 7, 21 or 27 after signup.
#[derive(Debug, Clone, Copy)]
pub struct ReminderInput<'a> {
    pub step: ReminderStep,
    /// A call is booked (status `booked`).
    pub booked: bool,
    /// The booked call's start, when stored.
    pub call_start_at: Option<NaiveDateTime>,
    pub company_name: &'a str,
    pub user_name: Option<&'a str>,
    pub deadline_at: NaiveDateTime,
    pub fee_cents: i64,
    pub app_url: &'a str,
}

pub fn render_reminder(input: &ReminderInput<'_>) -> RenderedEmail {
    let company = company_or_default(input.company_name);
    let deadline = format_local_date(input.deadline_at);
    let fee = format_money(input.fee_cents);
    let url = link(input.app_url, BOOKING_PATH);
    if input.booked {
        let subject = match input.step {
            ReminderStep::Day7 => "Your Granite Manager onboarding call is booked".to_string(),
            ReminderStep::Day21 | ReminderStep::Day27 => {
                "Reminder: your Granite Manager onboarding call is booked".to_string()
            }
        };
        let when = input
            .call_start_at
            .map(|at| format!(" for {}", format_local_time(at)))
            .unwrap_or_default();
        return render(
            subject,
            input.user_name,
            company,
            |e| {
                vec![
                    format!(
                        "The onboarding call for {} is booked{when}. You can see or change it in Granite Manager.",
                        e(company)
                    ),
                    format!(
                        "Once the call has taken place, the onboarding fee of {fee} is waived. If you need to move it, pick a new time before {deadline}."
                    ),
                ]
            },
            "See your call",
            &url,
        );
    }
    let subject = match input.step {
        ReminderStep::Day7 => "Book your Granite Manager onboarding call".to_string(),
        ReminderStep::Day21 => format!("Reminder: book your onboarding call by {deadline}"),
        ReminderStep::Day27 => format!("Last reminder: book your onboarding call by {deadline}"),
    };
    render(
        subject,
        input.user_name,
        company,
        |e| {
            vec![
                format!(
                    "The Granite Manager plan for {} includes an onboarding call with our team. Complete it by {deadline} and the onboarding fee of {fee} is waived.",
                    e(company)
                ),
                format!(
                    "The deadline moves one day later for each day our calendar has no open time within the next {SLOT_WINDOW_BUSINESS_DAYS} business days, and if we cancel or move your call, it moves to match."
                ),
                format!(
                    "Need more time? You can ask for one {EXTENSION_DAYS}-day extension: reply to this email or write to {SUPPORT_EMAIL}."
                ),
                format!(
                    "If you request cancellation of your subscription before {deadline}, the onboarding fee is not charged."
                ),
            ]
        },
        "Book your call",
        &url,
    )
}

/// The fee notice, at least 7 days before the charge.
#[derive(Debug, Clone, Copy)]
pub struct NoticeInput<'a> {
    pub company_name: &'a str,
    pub user_name: Option<&'a str>,
    pub fee_cents: i64,
    /// The deadline that passed.
    pub deadline_at: NaiveDateTime,
    /// When the fee is charged.
    pub charge_at: NaiveDateTime,
    /// `Visa ending in 4242`, when known.
    pub payment_method: Option<&'a str>,
    /// A booked call ends at or before `charge_at` (the rules then hold the
    /// fee and complete the commitment).
    pub booked_before_charge: bool,
    pub app_url: &'a str,
}

pub fn render_notice(input: &NoticeInput<'_>) -> RenderedEmail {
    let company = company_or_default(input.company_name);
    let deadline = format_local_date(input.deadline_at);
    let charge_date = format_local_date(input.charge_at);
    let fee = format_money(input.fee_cents);
    let method = input
        .payment_method
        .map(str::trim)
        .filter(|method| !method.is_empty())
        .unwrap_or("the payment method on file");
    let url = link(input.app_url, TERMS_PATH);
    render(
        format!("Action needed: onboarding fee of {fee} will be charged on {charge_date}"),
        input.user_name,
        company,
        |e| {
            let mut paragraphs = vec![
                format!(
                    "The onboarding call for {} was not completed by {deadline}, the onboarding deadline.",
                    e(company)
                ),
                format!(
                    "On {charge_date}, the onboarding fee of {fee} plus applicable sales tax will be charged to {}.",
                    e(method)
                ),
                format!(
                    "To avoid the fee, complete an onboarding call before {charge_date} (we mark it complete once it has taken place), or request cancellation of your subscription before then."
                ),
            ];
            if input.booked_before_charge {
                paragraphs.push(format!(
                    "You have an onboarding call booked. If your call has taken place before {charge_date}, the fee is not charged."
                ));
            }
            paragraphs.push(format!(
                "If you believe this fee is wrong, reply to this email before {charge_date} and we will review it before charging."
            ));
            paragraphs.push(
                "The fee will appear as a separate invoice, not part of your monthly subscription."
                    .to_string(),
            );
            paragraphs.push(
                "Section 11 of the Terms of Service explains the onboarding commitment."
                    .to_string(),
            );
            paragraphs
        },
        "Read the Terms, Section 11",
        &url,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP_URL: &str = "https://app.example.com/";

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn reminder(step: ReminderStep, booked: bool) -> RenderedEmail {
        render_reminder(&ReminderInput {
            step,
            booked,
            call_start_at: None,
            company_name: "Rock & <Stone>",
            user_name: Some("<b>Ann</b>"),
            deadline_at: at("2026-11-20 15:00:00"),
            fee_cents: 60_000,
            app_url: APP_URL,
        })
    }

    fn notice(payment_method: Option<&str>, booked_before_charge: bool) -> RenderedEmail {
        render_notice(&NoticeInput {
            company_name: "Rock & <Stone>",
            user_name: None,
            fee_cents: 123_450,
            deadline_at: at("2026-11-20 15:00:00"),
            charge_at: at("2026-11-27 15:00:00"),
            payment_method,
            booked_before_charge,
            app_url: APP_URL,
        })
    }

    fn all(email: &RenderedEmail) -> String {
        format!("{}\n{}\n{}", email.subject, email.html, email.text)
    }

    fn assert_clean(email: &RenderedEmail) {
        let all = all(email);
        assert!(
            !all.contains('\u{2014}') && !all.contains('\u{2013}'),
            "{all}"
        );
        assert_eq!(email.html.matches("href=").count(), 1);
        assert_eq!(email.text.matches("https://").count(), 1);
        assert!(!email.html.contains("<Stone>") && !email.html.contains("<b>"));
        assert!(email.html.contains("Rock &amp; &lt;Stone&gt;"));
        assert!(email.text.contains("Rock & <Stone>"));
    }

    #[test]
    fn a_pending_reminder_states_deadline_fee_tolling_and_the_extension() {
        let email = reminder(ReminderStep::Day7, false);
        assert_eq!(email.subject, "Book your Granite Manager onboarding call");
        assert!(email.text.starts_with("Hi <b>Ann</b>,"));
        assert!(email.html.contains("Hi &lt;b&gt;Ann&lt;/b&gt;,"));
        assert!(email.text.contains(
            "Complete it by November 20, 2026 and the onboarding fee of $600.00 is waived."
        ));
        assert!(email.text.contains("within the next 5 business days"));
        assert!(
            email
                .text
                .contains("if we cancel or move your call, it moves to match")
        );
        assert!(email.text.contains(
            "You can ask for one 14-day extension: reply to this email or write to support@granite-manager.com."
        ));
        assert!(email.text.contains(
            "If you request cancellation of your subscription before November 20, 2026, the onboarding fee is not charged."
        ));
        assert!(
            email
                .text
                .contains("Book your call: https://app.example.com/employee?book_onboarding=1")
        );
        assert!(
            email
                .html
                .contains("href=\"https://app.example.com/employee?book_onboarding=1\"")
        );
        assert_clean(&email);
    }

    #[test]
    fn later_reminders_name_the_deadline_in_the_subject() {
        assert_eq!(
            reminder(ReminderStep::Day21, false).subject,
            "Reminder: book your onboarding call by November 20, 2026"
        );
        assert_eq!(
            reminder(ReminderStep::Day27, false).subject,
            "Last reminder: book your onboarding call by November 20, 2026"
        );
        for step in [ReminderStep::Day7, ReminderStep::Day21, ReminderStep::Day27] {
            assert_clean(&reminder(step, false));
            assert_clean(&reminder(step, true));
        }
    }

    #[test]
    fn a_booked_reminder_says_it_is_booked_without_a_time() {
        let email = reminder(ReminderStep::Day21, true);
        assert_eq!(
            email.subject,
            "Reminder: your Granite Manager onboarding call is booked"
        );
        assert!(email.text.contains(
            "The onboarding call for Rock & <Stone> is booked. You can see or change it in Granite Manager."
        ));
        assert!(
            email
                .text
                .contains("the onboarding fee of $600.00 is waived")
        );
        assert!(
            email
                .text
                .contains("See your call: https://app.example.com/employee?book_onboarding=1")
        );
        assert!(!email.text.contains("15:00") && !email.text.contains("3:00"));
        assert_clean(&email);
    }

    #[test]
    fn the_notice_states_amount_dates_method_and_how_to_avoid_or_dispute() {
        let email = notice(Some("Visa ending in 4242"), false);
        assert_eq!(
            email.subject,
            "Action needed: onboarding fee of $1,234.50 will be charged on November 27, 2026"
        );
        assert!(email.text.starts_with("Hello,"));
        assert!(
            email
                .text
                .contains("was not completed by November 20, 2026")
        );
        assert!(email.text.contains(
            "On November 27, 2026, the onboarding fee of $1,234.50 plus applicable sales tax will be charged to Visa ending in 4242."
        ));
        assert!(email.text.contains(
            "complete an onboarding call before November 27, 2026 (we mark it complete once it has taken place), or request cancellation"
        ));
        assert!(
            email
                .text
                .contains("reply to this email before November 27, 2026")
        );
        assert!(email.text.contains("separate invoice"));
        assert!(
            email
                .text
                .contains("Read the Terms, Section 11: https://app.example.com/terms")
        );
        assert!(!email.text.contains("You have an onboarding call booked"));
        assert_clean(&email);
    }

    #[test]
    fn a_booked_reminder_names_the_call_time_in_the_signup_zone() {
        let email = render_reminder(&ReminderInput {
            step: ReminderStep::Day7,
            booked: true,
            call_start_at: Some(at("2026-11-05 01:00:00")),
            company_name: "Granite Co",
            user_name: None,
            deadline_at: at("2026-11-21 03:00:00"),
            fee_cents: 60_000,
            app_url: APP_URL,
        });
        assert!(email.text.contains(
            "The onboarding call for Granite Co is booked for Wed, Nov 4 at 8:00 PM EST."
        ));
        assert!(
            email
                .text
                .contains("pick a new time before November 20, 2026.")
        );
    }

    #[test]
    fn dates_are_the_local_day_not_the_utc_day() {
        let late = render_reminder(&ReminderInput {
            step: ReminderStep::Day21,
            booked: false,
            call_start_at: None,
            company_name: "Granite Co",
            user_name: None,
            deadline_at: at("2026-11-21 03:00:00"),
            fee_cents: 60_000,
            app_url: APP_URL,
        });
        assert_eq!(
            late.subject,
            "Reminder: book your onboarding call by November 20, 2026"
        );
        assert!(!all(&late).contains("November 21"));
        let notice = render_notice(&NoticeInput {
            company_name: "Granite Co",
            user_name: None,
            fee_cents: 60_000,
            deadline_at: at("2026-11-21 03:00:00"),
            charge_at: at("2026-11-28 02:00:00"),
            payment_method: None,
            booked_before_charge: false,
            app_url: APP_URL,
        });
        assert_eq!(
            notice.subject,
            "Action needed: onboarding fee of $600.00 will be charged on November 27, 2026"
        );
        assert!(
            notice
                .text
                .contains("was not completed by November 20, 2026")
        );
        assert!(
            notice
                .text
                .contains("On November 27, 2026, the onboarding fee")
        );
        assert!(!all(&notice).contains("November 21") && !all(&notice).contains("November 28"));
    }

    #[test]
    fn the_booked_call_line_names_the_local_charge_date() {
        let email = render_notice(&NoticeInput {
            company_name: "Granite Co",
            user_name: None,
            fee_cents: 60_000,
            deadline_at: at("2026-11-21 03:00:00"),
            charge_at: at("2026-11-28 02:00:00"),
            payment_method: None,
            booked_before_charge: true,
            app_url: APP_URL,
        });
        assert!(email.text.contains(
            "If your call has taken place before November 27, 2026, the fee is not charged."
        ));
        assert!(email.html.contains(
            "If your call has taken place before November 27, 2026, the fee is not charged."
        ));
        assert!(!all(&email).contains("November 28"));
    }

    #[test]
    fn the_notice_names_a_booked_call_and_falls_back_to_the_method_on_file() {
        let email = notice(None, true);
        assert!(
            email
                .text
                .contains("will be charged to the payment method on file.")
        );
        assert!(email.text.contains(
            "You have an onboarding call booked. If your call has taken place before November 27, 2026, the fee is not charged."
        ));
        assert_clean(&email);
        let escaped = notice(Some("<i>Card</i>"), false);
        assert!(escaped.html.contains("&lt;i&gt;Card&lt;/i&gt;"));
        assert!(!escaped.html.contains("<i>"));
    }
}
