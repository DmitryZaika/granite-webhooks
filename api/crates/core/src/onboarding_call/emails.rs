//! The onboarding emails to a company's Managers: the reminders on days 7,
//! 21 and 27 and the advance payment notice (Terms section 11).
//!
//! Onboarding is free. When it is not completed by the deadline, the next
//! subscription charge at least 7 days after the notice also includes the
//! following month in advance, which a credit then covers.
//!
//! Subjects, HTML (escaped) and plain-text parts, in the billing emails'
//! look. Each email has one link. Every date is the buyer's calendar day
//! and a call's time carries its zone (`SIGNUP_TZ`).

use super::commitment::{EXTENSION_DAYS, NOTICE_DAYS, ReminderStep, SLOT_WINDOW_BUSINESS_DAYS};
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

/// A reminder on day 7, 21 or 27 after signup (before the deadline only).
#[derive(Debug, Clone, Copy)]
pub struct ReminderInput<'a> {
    pub step: ReminderStep,
    /// A call is booked (status `booked`).
    pub booked: bool,
    /// The booked call's start, when stored.
    pub call_start_at: Option<NaiveDateTime>,
    /// The booked call's end, when stored: it must end by the deadline.
    pub call_end_at: Option<NaiveDateTime>,
    pub company_name: &'a str,
    pub user_name: Option<&'a str>,
    pub deadline_at: NaiveDateTime,
    /// The advance the signup consent stated (one month at the plan chosen,
    /// before tax); the notice fixes the charged amount.
    pub advance_cents: i64,
    pub app_url: &'a str,
}

/// What the advance payment is, with the amount the signup stated.
fn advance_sentence(advance: &str) -> String {
    format!(
        "If onboarding is not completed by then, we email a notice, and your next subscription charge at least {NOTICE_DAYS} days after it also includes the following month in advance: one month of your subscription ({advance} with your plan at signup) plus applicable sales tax. That month is then not charged again, so you pay nothing extra, only one month earlier."
    )
}

pub fn render_reminder(input: &ReminderInput<'_>) -> RenderedEmail {
    let company = company_or_default(input.company_name);
    let deadline = format_local_date(input.deadline_at);
    let advance = format_money(input.advance_cents);
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
        let late = input.call_end_at.is_some_and(|end| end > input.deadline_at);
        let ends = input.call_end_at.map_or_else(
            || format!("The call must end by {deadline}, your onboarding deadline."),
            |end| {
                let relation = if late { "after" } else { "by" };
                format!(
                    "Your call ends {}, {relation} your onboarding deadline of {deadline}.",
                    format_local_time(end)
                )
            },
        );
        return render(
            subject,
            input.user_name,
            company,
            |e| {
                let mut paragraphs = vec![
                    format!(
                        "The onboarding call for {} is booked{when}. You can see or change it in Granite Manager.",
                        e(company)
                    ),
                    format!(
                        "Onboarding is free and complete once your call has taken place. {ends}"
                    ),
                    advance_sentence(&advance),
                ];
                if late {
                    paragraphs.push(
                        "Because your call ends after the deadline, we will send that notice. Your call still avoids the advance payment if it ends before the charge date in the notice."
                            .to_string(),
                    );
                }
                paragraphs.push(format!(
                    "If you need to move your call, pick a new time that ends by {deadline}."
                ));
                paragraphs
            },
            "See your call",
            &url,
        );
    }
    let subject = match input.step {
        ReminderStep::Day7 => "Book your free Granite Manager onboarding call".to_string(),
        ReminderStep::Day21 => {
            format!("Reminder: complete your free onboarding call by {deadline}")
        }
        ReminderStep::Day27 => {
            format!("Last reminder: complete your free onboarding call by {deadline}")
        }
    };
    render(
        subject,
        input.user_name,
        company,
        |e| {
            vec![
                format!(
                    "The Granite Manager plan for {} includes a free onboarding call with our team. Book a call that ends by {deadline}, your onboarding deadline.",
                    e(company)
                ),
                advance_sentence(&advance),
                format!(
                    "The deadline moves one day later for each day our calendar has no open time within the next {SLOT_WINDOW_BUSINESS_DAYS} business days, and if we cancel or move your call, it moves to match."
                ),
                format!(
                    "Need more time? You can ask for one {EXTENSION_DAYS}-day extension: reply to this email or write to {SUPPORT_EMAIL}."
                ),
                "If you request cancellation of your subscription before your first charge, nothing is charged at all."
                    .to_string(),
            ]
        },
        "Book your call",
        &url,
    )
}

/// The advance payment notice, at least 7 days before the charge.
#[derive(Debug, Clone, Copy)]
pub struct NoticeInput<'a> {
    pub company_name: &'a str,
    pub user_name: Option<&'a str>,
    /// The advance: one month of the live subscription, before tax.
    pub advance_cents: i64,
    /// The deadline that passed.
    pub deadline_at: NaiveDateTime,
    /// The subscription charge that also takes the advance.
    pub charge_at: NaiveDateTime,
    /// The month the advance pays for (credited to the invoice of
    /// `prepaid_from` once paid).
    pub prepaid_from: NaiveDateTime,
    pub prepaid_until: NaiveDateTime,
    /// `Visa ending in 4242`, when known.
    pub payment_method: Option<&'a str>,
    /// A booked call ends at or before `charge_at` (the rules then hold the
    /// advance and complete the commitment once it took place).
    pub booked_before_charge: bool,
    pub app_url: &'a str,
}

pub fn render_notice(input: &NoticeInput<'_>) -> RenderedEmail {
    let company = company_or_default(input.company_name);
    let deadline = format_local_date(input.deadline_at);
    let charge_date = format_local_date(input.charge_at);
    let charge_time = format_local_time(input.charge_at);
    let from = format_local_date(input.prepaid_from);
    let until = format_local_date(input.prepaid_until);
    let advance = format_money(input.advance_cents);
    let method = input
        .payment_method
        .map(str::trim)
        .filter(|method| !method.is_empty())
        .unwrap_or("the payment method on file");
    let url = link(input.app_url, TERMS_PATH);
    render(
        format!(
            "Action needed: your charge on {charge_date} will include a {advance} advance payment"
        ),
        input.user_name,
        company,
        |e| {
            let mut paragraphs = vec![
                format!(
                    "Onboarding for {} was not completed by {deadline}, the onboarding deadline.",
                    e(company)
                ),
                format!(
                    "So your subscription charge on {charge_date} will also include the following month in advance: an advance payment of {advance} plus applicable sales tax, charged to {}. It covers {from} to {until}.",
                    e(method)
                ),
                format!(
                    "You pay nothing extra overall, only one month earlier: once that charge is paid, a credit of the same amount pays your invoice on {from}. If your plan grows by then, only the difference is charged."
                ),
                format!(
                    "To avoid it, have an onboarding call that ends before {charge_time} (book it in Granite Manager; it counts once it has taken place). If you request cancellation of your subscription before then, the subscription ends and that charge, with the advance payment, is not made."
                ),
            ];
            if input.booked_before_charge {
                paragraphs.push(
                    "You have an onboarding call booked that ends before then. Once it has taken place, the advance payment is not charged."
                        .to_string(),
                );
            }
            paragraphs.push(format!(
                "If you think the advance payment should not apply, reply to this email or write to {SUPPORT_EMAIL} before {charge_date}, and we will review it before charging."
            ));
            paragraphs.push(
                "Section 11 of the Terms of Service explains the advance payment.".to_string(),
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
            call_end_at: None,
            company_name: "Rock & <Stone>",
            user_name: Some("<b>Ann</b>"),
            deadline_at: at("2026-11-20 15:00:00"),
            advance_cents: 60_000,
            app_url: APP_URL,
        })
    }

    fn notice(payment_method: Option<&str>, booked_before_charge: bool) -> RenderedEmail {
        render_notice(&NoticeInput {
            company_name: "Rock & <Stone>",
            user_name: None,
            advance_cents: 123_450,
            deadline_at: at("2026-11-20 15:00:00"),
            charge_at: at("2026-11-27 15:00:00"),
            prepaid_from: at("2026-12-27 15:00:00"),
            prepaid_until: at("2027-01-27 15:00:00"),
            payment_method,
            booked_before_charge,
            app_url: APP_URL,
        })
    }

    fn all(email: &RenderedEmail) -> String {
        format!("{}\n{}\n{}", email.subject, email.html, email.text)
    }

    /// No dash, no onboarding fee (onboarding is free), one link, escaped.
    fn assert_clean(email: &RenderedEmail) {
        let all = all(email);
        assert!(
            !all.contains('\u{2014}') && !all.contains('\u{2013}'),
            "{all}"
        );
        assert!(!all.to_lowercase().contains("fee"), "{all}");
        assert_eq!(email.html.matches("href=").count(), 1);
        assert_eq!(email.text.matches("https://").count(), 1);
        assert!(!email.html.contains("<Stone>") && !email.html.contains("<b>"));
        assert!(email.html.contains("Rock &amp; &lt;Stone&gt;"));
        assert!(email.text.contains("Rock & <Stone>"));
    }

    #[test]
    fn a_pending_reminder_is_exact() {
        let email = reminder(ReminderStep::Day7, false);
        assert_eq!(
            email.subject,
            "Book your free Granite Manager onboarding call"
        );
        assert_eq!(
            email.text,
            "Hi <b>Ann</b>,\n\n\
             The Granite Manager plan for Rock & <Stone> includes a free onboarding call with our team. Book a call that ends by November 20, 2026, your onboarding deadline.\n\n\
             If onboarding is not completed by then, we email a notice, and your next subscription charge at least 7 days after it also includes the following month in advance: one month of your subscription ($600.00 with your plan at signup) plus applicable sales tax. That month is then not charged again, so you pay nothing extra, only one month earlier.\n\n\
             The deadline moves one day later for each day our calendar has no open time within the next 5 business days, and if we cancel or move your call, it moves to match.\n\n\
             Need more time? You can ask for one 14-day extension: reply to this email or write to support@granite-manager.com.\n\n\
             If you request cancellation of your subscription before your first charge, nothing is charged at all.\n\n\
             Book your call: https://app.example.com/employee?book_onboarding=1\n\n\
             This email was sent to managers of Rock & <Stone>."
        );
        assert!(email.html.contains("Hi &lt;b&gt;Ann&lt;/b&gt;,"));
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
            "Reminder: complete your free onboarding call by November 20, 2026"
        );
        assert_eq!(
            reminder(ReminderStep::Day27, false).subject,
            "Last reminder: complete your free onboarding call by November 20, 2026"
        );
        for step in [ReminderStep::Day7, ReminderStep::Day21, ReminderStep::Day27] {
            assert_clean(&reminder(step, false));
            assert_clean(&reminder(step, true));
        }
    }

    #[test]
    fn a_booked_reminder_without_the_call_times_states_the_rule() {
        let email = reminder(ReminderStep::Day21, true);
        assert_eq!(
            email.subject,
            "Reminder: your Granite Manager onboarding call is booked"
        );
        assert_eq!(
            email.text,
            "Hi <b>Ann</b>,\n\n\
             The onboarding call for Rock & <Stone> is booked. You can see or change it in Granite Manager.\n\n\
             Onboarding is free and complete once your call has taken place. The call must end by November 20, 2026, your onboarding deadline.\n\n\
             If onboarding is not completed by then, we email a notice, and your next subscription charge at least 7 days after it also includes the following month in advance: one month of your subscription ($600.00 with your plan at signup) plus applicable sales tax. That month is then not charged again, so you pay nothing extra, only one month earlier.\n\n\
             If you need to move your call, pick a new time that ends by November 20, 2026.\n\n\
             See your call: https://app.example.com/employee?book_onboarding=1\n\n\
             This email was sent to managers of Rock & <Stone>."
        );
        assert_clean(&email);
    }

    fn booked_at(start: &str, end: &str) -> RenderedEmail {
        render_reminder(&ReminderInput {
            step: ReminderStep::Day7,
            booked: true,
            call_start_at: Some(at(start)),
            call_end_at: Some(at(end)),
            company_name: "Granite Co",
            user_name: None,
            deadline_at: at("2026-11-21 04:59:59"),
            advance_cents: 60_000,
            app_url: APP_URL,
        })
    }

    #[test]
    fn a_booked_reminder_names_the_call_times_in_the_signup_zone() {
        let email = booked_at("2026-11-05 01:00:00", "2026-11-05 01:30:00");
        assert!(email.text.contains(
            "The onboarding call for Granite Co is booked for Wed, Nov 4 at 8:00 PM EST."
        ));
        assert!(email.text.contains(
            "Onboarding is free and complete once your call has taken place. Your call ends Wed, Nov 4 at 8:30 PM EST, by your onboarding deadline of November 20, 2026."
        ));
        assert!(
            !email
                .text
                .contains("Because your call ends after the deadline")
        );
        assert!(
            email
                .text
                .contains("pick a new time that ends by November 20, 2026.")
        );
    }

    #[test]
    fn a_call_ending_after_the_deadline_is_told_it_gets_the_notice_but_still_counts() {
        let email = booked_at("2026-11-21 04:45:00", "2026-11-21 05:15:00");
        assert!(email.text.contains(
            "Your call ends Sat, Nov 21 at 12:15 AM EST, after your onboarding deadline of November 20, 2026."
        ));
        assert!(email.text.contains(
            "Because your call ends after the deadline, we will send that notice. Your call still avoids the advance payment if it ends before the charge date in the notice."
        ));
    }

    #[test]
    fn the_notice_is_exact() {
        let email = notice(Some("Visa ending in 4242"), false);
        assert_eq!(
            email.subject,
            "Action needed: your charge on November 27, 2026 will include a $1,234.50 advance payment"
        );
        assert_eq!(
            email.text,
            "Hello,\n\n\
             Onboarding for Rock & <Stone> was not completed by November 20, 2026, the onboarding deadline.\n\n\
             So your subscription charge on November 27, 2026 will also include the following month in advance: an advance payment of $1,234.50 plus applicable sales tax, charged to Visa ending in 4242. It covers December 27, 2026 to January 27, 2027.\n\n\
             You pay nothing extra overall, only one month earlier: once that charge is paid, a credit of the same amount pays your invoice on December 27, 2026. If your plan grows by then, only the difference is charged.\n\n\
             To avoid it, have an onboarding call that ends before Fri, Nov 27 at 10:00 AM EST (book it in Granite Manager; it counts once it has taken place). If you request cancellation of your subscription before then, the subscription ends and that charge, with the advance payment, is not made.\n\n\
             If you think the advance payment should not apply, reply to this email or write to support@granite-manager.com before November 27, 2026, and we will review it before charging.\n\n\
             Section 11 of the Terms of Service explains the advance payment.\n\n\
             Read the Terms, Section 11: https://app.example.com/terms\n\n\
             This email was sent to managers of Rock & <Stone>."
        );
        assert_clean(&email);
    }

    #[test]
    fn dates_are_the_local_day_not_the_utc_day() {
        let late = render_reminder(&ReminderInput {
            step: ReminderStep::Day21,
            booked: false,
            call_start_at: None,
            call_end_at: None,
            company_name: "Granite Co",
            user_name: None,
            deadline_at: at("2026-11-21 03:00:00"),
            advance_cents: 60_000,
            app_url: APP_URL,
        });
        assert_eq!(
            late.subject,
            "Reminder: complete your free onboarding call by November 20, 2026"
        );
        assert!(!all(&late).contains("November 21"));
        let notice = render_notice(&NoticeInput {
            company_name: "Granite Co",
            user_name: None,
            advance_cents: 60_000,
            deadline_at: at("2026-11-21 03:00:00"),
            charge_at: at("2026-11-28 02:00:00"),
            prepaid_from: at("2026-12-28 02:00:00"),
            prepaid_until: at("2027-01-28 02:00:00"),
            payment_method: None,
            booked_before_charge: false,
            app_url: APP_URL,
        });
        assert_eq!(
            notice.subject,
            "Action needed: your charge on November 27, 2026 will include a $600.00 advance payment"
        );
        assert!(
            notice
                .text
                .contains("was not completed by November 20, 2026")
        );
        assert!(
            notice
                .text
                .contains("It covers December 27, 2026 to January 27, 2027.")
        );
        assert!(
            notice
                .text
                .contains("an onboarding call that ends before Fri, Nov 27 at 9:00 PM EST")
        );
        assert!(!all(&notice).contains("November 21") && !all(&notice).contains("November 28"));
    }

    #[test]
    fn the_notice_names_a_booked_call_and_falls_back_to_the_method_on_file() {
        let email = notice(None, true);
        assert!(
            email
                .text
                .contains("charged to the payment method on file. It covers")
        );
        assert!(email.text.contains(
            "You have an onboarding call booked that ends before then. Once it has taken place, the advance payment is not charged."
        ));
        assert_clean(&email);
        let escaped = notice(Some("<i>Card</i>"), false);
        assert!(escaped.html.contains("&lt;i&gt;Card&lt;/i&gt;"));
        assert!(!escaped.html.contains("<i>"));
    }
}
