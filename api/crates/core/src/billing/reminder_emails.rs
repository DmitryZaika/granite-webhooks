//! The billing reminder emails: subjects, HTML (escaped) and text parts.
//!
//! The one link is the billing page: a normal sign-in is required, nothing in
//! the email grants access. Port of the CRM's `billingReminderEmails.ts`, plus
//! the first-charge notice (transactional only, no marketing).

use super::catalog::SIGNUP_TZ;
use super::reminders::{DunningKind, FirstChargeQuote, ReminderStep};
use super::{BILLING_PATH, format_billing_date};
use crate::onboarding_call::commitment::{Commitment, Status};
use crate::signup::consent::long_date;
use chrono::{NaiveDateTime, TimeZone};
use std::fmt::Write;

/// Subject, HTML and plain-text parts of one email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedEmail {
    pub subject: String,
    pub html: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ReminderEmailInput<'a> {
    pub kind: DunningKind,
    pub step: ReminderStep,
    pub company_name: &'a str,
    pub user_name: Option<&'a str>,
    /// When access is (or was) paused; `None` once the cycle is settled.
    pub pauses_at: Option<NaiveDateTime>,
    pub app_url: &'a str,
}

struct Copy {
    subject: &'static str,
    paragraphs: Vec<String>,
    action: &'static str,
}

pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

const UPDATE_CARD: &str = "Update payment method";
const SUBSCRIBE: &str = "Subscribe";
const VIEW_BILLING: &str = "View billing";
const PAUSED_SUBJECT: &str = "Access to Granite Manager is paused";
const TOMORROW_SUBJECT: &str = "Access to Granite Manager will be paused tomorrow";
pub(crate) const SUPPORT_EMAIL: &str = "support@granite-manager.com";

fn payment_failed_copy(step: ReminderStep, company: &str, date: &str) -> Option<Copy> {
    let (subject, paragraphs, action) = match step {
        ReminderStep::Started => (
            "Action needed: your Granite Manager payment did not go through",
            vec![
                format!(
                    "We could not process the latest subscription payment for {company}. Please update your card to keep your account active."
                ),
                format!(
                    "If the payment is not completed, access for your team will be paused on {date}."
                ),
            ],
            UPDATE_CARD,
        ),
        ReminderStep::ThreeDaysLeft => (
            "3 days left to update your payment",
            vec![
                format!(
                    "The payment for {company} is still outstanding. Access for your team will be paused on {date}."
                ),
                "Update your card now to avoid any interruption.".to_string(),
            ],
            UPDATE_CARD,
        ),
        ReminderStep::PauseTomorrow => (
            TOMORROW_SUBJECT,
            vec![
                format!(
                    "The payment for {company} is still outstanding. Access for your team will be paused tomorrow, {date}."
                ),
                "Please update your card today to keep working without interruption.".to_string(),
            ],
            UPDATE_CARD,
        ),
        ReminderStep::Paused => (
            PAUSED_SUBJECT,
            vec![
                format!(
                    "We have not received the payment for {company}, so access for your team was paused on {date}."
                ),
                "Your data is safe. As soon as the payment goes through, access is restored."
                    .to_string(),
            ],
            "Pay now",
        ),
        ReminderStep::Resolved => (
            "Payment received, access restored",
            vec![
                format!("Thank you, we received the payment for {company}."),
                "Your Granite Manager account is active and your team has full access.".to_string(),
            ],
            VIEW_BILLING,
        ),
        ReminderStep::Upcoming => return None,
    };
    Some(Copy {
        subject,
        paragraphs,
        action,
    })
}

fn first_payment_copy(step: ReminderStep, company: &str, date: &str) -> Option<Copy> {
    let (subject, paragraphs, action) = match step {
        ReminderStep::Started => (
            "Subscribe to keep using Granite Manager",
            vec![
                format!("{company} now needs a Granite Manager subscription."),
                format!("Please subscribe by {date} to keep access for your team."),
            ],
            SUBSCRIBE,
        ),
        ReminderStep::ThreeDaysLeft => (
            "3 days left to subscribe",
            vec![
                format!(
                    "Access for your team will be paused on {date} unless {company} subscribes."
                ),
                "Subscribe now to avoid any interruption.".to_string(),
            ],
            SUBSCRIBE,
        ),
        ReminderStep::PauseTomorrow => (
            TOMORROW_SUBJECT,
            vec![
                format!(
                    "{company} has not subscribed yet. Access for your team will be paused tomorrow, {date}."
                ),
                "Subscribe today to keep working without interruption.".to_string(),
            ],
            SUBSCRIBE,
        ),
        ReminderStep::Paused => (
            PAUSED_SUBJECT,
            vec![
                format!(
                    "{company} has not subscribed, so access for your team was paused on {date}."
                ),
                "Your data is safe. Subscribe to restore access right away.".to_string(),
            ],
            SUBSCRIBE,
        ),
        ReminderStep::Resolved => (
            "Your Granite Manager subscription is active",
            vec![
                format!("Thank you, {company} is subscribed."),
                "Your team has full access to Granite Manager.".to_string(),
            ],
            VIEW_BILLING,
        ),
        ReminderStep::Upcoming => return None,
    };
    Some(Copy {
        subject,
        paragraphs,
        action,
    })
}

fn reminder_copy(
    kind: DunningKind,
    step: ReminderStep,
    company: &str,
    date: Option<&str>,
) -> Option<Copy> {
    let date = date.unwrap_or("soon");
    match kind {
        DunningKind::FirstPayment => first_payment_copy(step, company, date),
        DunningKind::PaymentFailed => payment_failed_copy(step, company, date),
        DunningKind::FirstCharge => None,
    }
}

fn billing_link(app_url: &str) -> String {
    format!("{}{BILLING_PATH}", app_url.trim_end_matches('/'))
}

pub(crate) fn greeting(user_name: Option<&str>) -> String {
    user_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map_or_else(|| "Hello,".to_string(), |name| format!("Hi {name},"))
}

pub(crate) fn company_or_default(name: &str) -> &str {
    let name = name.trim();
    if name.is_empty() {
        "your company"
    } else {
        name
    }
}

pub(crate) fn footer(company: &str) -> String {
    format!(
        "Sign in with your usual account to open the billing page. This email was sent to managers of {company}."
    )
}

pub(crate) fn html_paragraph(body: &str) -> String {
    format!("<p style=\"margin:0 0 16px;font-size:15px;line-height:22px;color:#1f2937\">{body}</p>")
}

pub(crate) fn html_button(link: &str, action: &str) -> String {
    format!(
        "<p style=\"margin:24px 0\"><a href=\"{}\" style=\"background:#111827;color:#ffffff;padding:12px 20px;border-radius:6px;text-decoration:none;font-size:15px;display:inline-block\">{}</a></p>",
        escape_html(link),
        escape_html(action)
    )
}

pub(crate) fn html_footer(company_html: &str) -> String {
    format!(
        "<p style=\"margin:0;font-size:13px;line-height:20px;color:#6b7280\">{}</p>",
        footer(company_html)
    )
}

pub(crate) fn wrap_html(parts: Vec<String>) -> String {
    let mut all =
        vec!["<div style=\"font-family:Arial,Helvetica,sans-serif;max-width:560px\">".to_string()];
    all.extend(parts);
    all.push("</div>".to_string());
    all.join("\n")
}

/// Escaped HTML plus a plain-text part. `None` for a kind with its own
/// renderer (`FirstCharge`) or a step the kind never sends.
pub fn render_reminder_email(input: &ReminderEmailInput<'_>) -> Option<RenderedEmail> {
    let company = company_or_default(input.company_name);
    let link = billing_link(input.app_url);
    let pause_date = input.pauses_at.map(format_billing_date);
    let plain = reminder_copy(input.kind, input.step, company, pause_date.as_deref())?;
    let company_html = escape_html(company);
    let date_html = pause_date.as_deref().map(escape_html);
    let html = reminder_copy(input.kind, input.step, &company_html, date_html.as_deref())?;
    let hello = greeting(input.user_name);
    let mut parts = vec![html_paragraph(&escape_html(&hello))];
    parts.extend(html.paragraphs.iter().map(|p| html_paragraph(p)));
    parts.push(html_button(&link, html.action));
    parts.push(html_footer(&company_html));
    let mut text = vec![hello];
    text.extend(plain.paragraphs);
    text.push(format!("{}: {link}", plain.action));
    text.push(footer(company));
    Some(RenderedEmail {
        subject: plain.subject.to_string(),
        html: wrap_html(parts),
        text: text.join("\n\n"),
    })
}

/// `$1,234.50`.
pub fn format_money(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let cents = cents.unsigned_abs();
    let digits = (cents / 100).to_string();
    let mut grouped = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("{sign}${grouped}.{:02}", cents % 100)
}

/// `November 20, 2026`: the buyer's calendar day ([`SIGNUP_TZ`]) of a UTC time.
pub fn format_local_date(at: NaiveDateTime) -> String {
    long_date(SIGNUP_TZ.from_utc_datetime(&at).date_naive())
}

/// `Tue, Nov 4 at 10:00 AM EST`: a UTC time in [`SIGNUP_TZ`], with its zone.
pub fn format_local_time(at: NaiveDateTime) -> String {
    SIGNUP_TZ
        .from_utc_datetime(&at)
        .format("%a, %b %-d at %-I:%M %p %Z")
        .to_string()
}

/// The onboarding call's status line in the first-charge email.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingLine {
    Complete,
    /// A call is booked; its start, when stored.
    Booked {
        start_at: Option<NaiveDateTime>,
    },
    /// Not booked yet: the deadline and the fee it avoids.
    NotBooked {
        deadline_at: NaiveDateTime,
        fee_cents: i64,
    },
    /// The deadline passed and the fee notice went out: the charge date.
    NoticeSent {
        charge_at: NaiveDateTime,
        fee_cents: i64,
    },
}

impl OnboardingLine {
    /// Completed, booked, pending or notice sent; nothing for other statuses.
    pub fn of(commitment: &Commitment) -> Option<Self> {
        match commitment.status {
            Status::Completed => Some(Self::Complete),
            Status::Booked => Some(Self::Booked {
                start_at: commitment.booking.as_ref().map(|call| call.start_at),
            }),
            Status::Pending => Some(Self::NotBooked {
                deadline_at: commitment.deadline_at,
                fee_cents: commitment.fee_cents,
            }),
            Status::NoticeSent => Some(commitment.charge_after.map_or(
                Self::NotBooked {
                    deadline_at: commitment.deadline_at,
                    fee_cents: commitment.fee_cents,
                },
                |charge_at| Self::NoticeSent {
                    charge_at,
                    fee_cents: commitment.fee_cents,
                },
            )),
            _ => None,
        }
    }

    pub fn text(self) -> String {
        match self {
            Self::Complete => "Onboarding: complete.".into(),
            Self::Booked { start_at: None } => "Your onboarding call is booked.".into(),
            Self::Booked {
                start_at: Some(start_at),
            } => format!(
                "Your onboarding call is booked for {}.",
                format_local_time(start_at)
            ),
            Self::NotBooked {
                deadline_at,
                fee_cents,
            } => format!(
                "Onboarding: not booked yet. Complete it by {} to avoid the onboarding fee of {}.",
                format_local_date(deadline_at),
                format_money(fee_cents)
            ),
            Self::NoticeSent {
                charge_at,
                fee_cents,
            } => format!(
                "Onboarding was not completed. The onboarding fee of {} will be charged on {}.",
                format_money(fee_cents),
                format_local_date(charge_at)
            ),
        }
    }
}

pub struct FirstChargeEmailInput<'a> {
    pub company_name: &'a str,
    pub user_name: Option<&'a str>,
    pub quote: &'a FirstChargeQuote,
    pub app_url: &'a str,
    /// The payments app's account page (`payments_account_url()`), where a
    /// Manager can use "Request cancellation".
    pub account_url: &'a str,
    /// The onboarding call's status, when the company has one to mention.
    pub onboarding: Option<OnboardingLine>,
}

/// The notice 7 days before the first charge of a self-serve subscription.
pub fn render_first_charge_email(input: &FirstChargeEmailInput<'_>) -> RenderedEmail {
    let company = company_or_default(input.company_name);
    let link = billing_link(input.app_url);
    let date = format_local_date(input.quote.charge_at);
    let amount = format_money(input.quote.amount_cents);
    let method = input
        .quote
        .payment_method
        .as_deref()
        .unwrap_or("the payment method on file");
    let intro = |company: &str, method: &str| {
        format!(
            "Your free period for {company} ends on {date}. On that day, {amount} plus applicable sales tax will be charged to {method}, and then every month on the same day until you cancel."
        )
    };
    let per_user = "Each active user above 10 adds $30.00 per month, counted on the charge date.";
    let cancel = format!(
        "To cancel before {date} and pay nothing, use Request cancellation on your account page ({}), or email {SUPPORT_EMAIL}.",
        input.account_url
    );
    let cancel_html = format!(
        "To cancel before {} and pay nothing, use <a href=\"{}\" style=\"color:#111827\">Request cancellation</a> on your account page, or email {SUPPORT_EMAIL}.",
        escape_html(&date),
        escape_html(input.account_url)
    );
    let change = "To change your payment method, open the billing page.";
    let hello = greeting(input.user_name);
    let company_html = escape_html(company);
    let mut items = String::new();
    for line in &input.quote.lines {
        let _ = write!(
            items,
            "<li style=\"margin:0 0 4px\">{}: {}</li>",
            escape_html(&line.label),
            format_money(line.amount_cents)
        );
    }
    let list_html = format!(
        "<ul style=\"margin:0 0 16px;padding-left:20px;font-size:15px;line-height:22px;color:#1f2937\">{items}</ul>"
    );
    let lines_text: Vec<String> = input
        .quote
        .lines
        .iter()
        .map(|line| format!("- {}: {}", line.label, format_money(line.amount_cents)))
        .collect();
    let onboarding = input.onboarding.map(OnboardingLine::text);
    let mut parts = vec![
        html_paragraph(&escape_html(&hello)),
        html_paragraph(&intro(&company_html, &escape_html(method))),
        html_paragraph("This includes:"),
        list_html,
        html_paragraph(per_user),
        html_paragraph(&cancel_html),
        html_paragraph(change),
    ];
    let mut text = vec![
        hello,
        intro(company, method),
        format!("This includes:\n{}", lines_text.join("\n")),
        per_user.to_string(),
        cancel,
        change.to_string(),
    ];
    if let Some(line) = onboarding {
        parts.push(html_paragraph(&escape_html(&line)));
        text.push(line);
    }
    parts.push(html_button(&link, VIEW_BILLING));
    parts.push(html_footer(&company_html));
    text.push(format!("{VIEW_BILLING}: {link}"));
    text.push(footer(company));
    RenderedEmail {
        subject: format!("Your first Granite Manager charge is on {date}"),
        html: wrap_html(parts),
        text: text.join("\n\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::reminders::ChargeLine;
    use crate::billing::test_store::at;

    const APP_URL: &str = "https://app.example.com";
    const ACCOUNT_URL: &str = "https://payments.example.com/account";

    fn pauses_at() -> NaiveDateTime {
        at("2026-10-08 15:00:00")
    }

    #[test]
    fn escapes_names_in_html_keeps_a_plain_text_part_links_only_to_billing() {
        let email = render_reminder_email(&ReminderEmailInput {
            kind: DunningKind::PaymentFailed,
            step: ReminderStep::Started,
            company_name: "Rock & <Stone> \"Co\"",
            user_name: Some("<script>alert(1)</script>"),
            pauses_at: Some(pauses_at()),
            app_url: "https://app.example.com/",
        })
        .unwrap();

        assert!(!email.html.contains("<script>"));
        assert!(email.html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(
            email
                .html
                .contains("Rock &amp; &lt;Stone&gt; &quot;Co&quot;")
        );
        assert!(
            email
                .html
                .contains("href=\"https://app.example.com/admin/billing\"")
        );
        assert!(email.text.contains("Hi <script>alert(1)</script>,"));
        assert!(
            email
                .text
                .contains("Update payment method: https://app.example.com/admin/billing")
        );
        assert!(email.text.contains("paused on October 8, 2026"));
    }

    #[test]
    fn every_email_has_one_billing_link_its_date_and_no_dash() {
        let steps = [
            ReminderStep::Started,
            ReminderStep::ThreeDaysLeft,
            ReminderStep::PauseTomorrow,
            ReminderStep::Paused,
            ReminderStep::Resolved,
        ];
        let mut subjects = std::collections::HashSet::new();
        for kind in [DunningKind::PaymentFailed, DunningKind::FirstPayment] {
            for step in steps {
                let email = render_reminder_email(&ReminderEmailInput {
                    kind,
                    step,
                    company_name: "Granite Co",
                    user_name: None,
                    pauses_at: (step != ReminderStep::Resolved).then(pauses_at),
                    app_url: APP_URL,
                })
                .unwrap();
                let all = format!("{}\n{}\n{}", email.subject, email.html, email.text);
                assert!(!all.contains('\u{2014}'));
                assert_eq!(email.html.matches("href=").count(), 1);
                assert_eq!(email.text.matches("https://").count(), 1);
                assert!(email.text.starts_with("Hello,"));
                if step != ReminderStep::Resolved {
                    assert!(email.text.contains("October 8, 2026"));
                }
                subjects.insert(format!("{}:{}", kind.as_str(), email.subject));
            }
        }
        assert_eq!(subjects.len(), 10);
    }

    #[test]
    fn a_company_asked_to_pay_is_told_to_subscribe_not_that_a_payment_failed() {
        let email = render_reminder_email(&ReminderEmailInput {
            kind: DunningKind::FirstPayment,
            step: ReminderStep::Started,
            company_name: "Granite Co",
            user_name: Some("Ann"),
            pauses_at: Some(pauses_at()),
            app_url: APP_URL,
        })
        .unwrap();
        assert_eq!(email.subject, "Subscribe to keep using Granite Manager");
        assert!(email.text.contains("Please subscribe by October 8, 2026"));
        assert!(
            email
                .text
                .contains("Subscribe: https://app.example.com/admin/billing")
        );
        assert!(!email.text.contains("payment"));
    }

    #[test]
    fn first_charge_has_its_own_renderer() {
        let input = ReminderEmailInput {
            kind: DunningKind::FirstCharge,
            step: ReminderStep::Upcoming,
            company_name: "Granite Co",
            user_name: None,
            pauses_at: None,
            app_url: APP_URL,
        };
        assert_eq!(render_reminder_email(&input), None);
    }

    fn quote() -> FirstChargeQuote {
        FirstChargeQuote {
            charge_at: at("2026-10-08 15:00:00"),
            amount_cents: 123_450,
            lines: vec![
                ChargeLine {
                    label: "Base plan, up to 10 users".into(),
                    amount_cents: 120_000,
                },
                ChargeLine {
                    label: "<b>Quotes add-on</b>".into(),
                    amount_cents: 3_450,
                },
            ],
            payment_method: Some("Visa ending in 4242".into()),
        }
    }

    #[test]
    fn the_first_charge_email_states_date_amount_and_lines() {
        let quote = quote();
        let email = render_first_charge_email(&FirstChargeEmailInput {
            company_name: "Rock & Co",
            user_name: Some("Ann"),
            quote: &quote,
            app_url: "https://app.example.com/",
            account_url: ACCOUNT_URL,
            onboarding: None,
        });

        assert_eq!(
            email.subject,
            "Your first Granite Manager charge is on October 8, 2026"
        );
        assert!(email.text.starts_with("Hi Ann,"));
        assert!(email.text.contains(
            "Your free period for Rock & Co ends on October 8, 2026. On that day, $1,234.50 plus applicable sales tax will be charged to Visa ending in 4242, and then every month on the same day until you cancel."
        ));
        assert!(email.text.contains(
            "This includes:\n- Base plan, up to 10 users: $1,200.00\n- <b>Quotes add-on</b>: $34.50"
        ));
        assert!(email.text.contains(
            "Each active user above 10 adds $30.00 per month, counted on the charge date."
        ));
        assert!(email.text.contains(
            "To cancel before October 8, 2026 and pay nothing, use Request cancellation on your account page (https://payments.example.com/account), or email support@granite-manager.com."
        ));
        assert!(email.html.contains(
            "use <a href=\"https://payments.example.com/account\" style=\"color:#111827\">Request cancellation</a> on your account page, or email support@granite-manager.com."
        ));
        assert!(
            email
                .text
                .contains("To change your payment method, open the billing page.")
        );
        assert!(
            email
                .text
                .contains("View billing: https://app.example.com/admin/billing")
        );
        assert!(email.html.contains("Rock &amp; Co"));
        assert!(email.html.contains(
            "<li style=\"margin:0 0 4px\">&lt;b&gt;Quotes add-on&lt;/b&gt;: $34.50</li>"
        ));
        assert!(!email.html.contains("<b>"));
        // The billing page and the account page.
        assert_eq!(email.html.matches("href=").count(), 2);
        let all = format!("{}\n{}\n{}", email.subject, email.html, email.text);
        assert!(!all.contains('\u{2014}') && !all.contains('\u{2013}'));
    }

    #[test]
    fn without_a_known_card_the_method_on_file_is_named() {
        let quote = FirstChargeQuote {
            payment_method: None,
            ..quote()
        };
        let email = render_first_charge_email(&FirstChargeEmailInput {
            company_name: " ",
            user_name: None,
            quote: &quote,
            app_url: APP_URL,
            account_url: ACCOUNT_URL,
            onboarding: None,
        });
        assert!(email.text.starts_with("Hello,"));
        assert!(
            email
                .text
                .contains("Your free period for your company ends")
        );
        assert!(
            email
                .text
                .contains("charged to the payment method on file, and then")
        );
    }

    fn onboarding_text(onboarding: Option<OnboardingLine>) -> RenderedEmail {
        let quote = quote();
        render_first_charge_email(&FirstChargeEmailInput {
            company_name: "Rock & Co",
            user_name: None,
            quote: &quote,
            app_url: APP_URL,
            account_url: ACCOUNT_URL,
            onboarding,
        })
    }

    #[test]
    fn the_first_charge_email_states_the_onboarding_status() {
        let complete = onboarding_text(Some(OnboardingLine::Complete));
        assert!(
            complete
                .text
                .contains("\n\nOnboarding: complete.\n\nView billing: ")
        );
        assert!(complete.html.contains(">Onboarding: complete.</p>"));
        let booked = onboarding_text(Some(OnboardingLine::Booked {
            start_at: Some(at("2026-11-04 15:00:00")),
        }));
        assert!(
            booked
                .text
                .contains("Your onboarding call is booked for Wed, Nov 4 at 10:00 AM EST.")
        );
        let unknown_time = onboarding_text(Some(OnboardingLine::Booked { start_at: None }));
        assert!(
            unknown_time
                .text
                .contains("Your onboarding call is booked.")
        );
        let pending = onboarding_text(Some(OnboardingLine::NotBooked {
            deadline_at: at("2026-11-21 03:00:00"),
            fee_cents: 60_000,
        }));
        assert!(pending.text.contains(
            "Onboarding: not booked yet. Complete it by November 20, 2026 to avoid the onboarding fee of $600.00."
        ));
        // The billing page button and the Request cancellation link.
        assert_eq!(pending.html.matches("href=").count(), 2);
        let noticed = onboarding_text(Some(OnboardingLine::NoticeSent {
            charge_at: at("2026-11-28 02:00:00"),
            fee_cents: 60_000,
        }));
        assert!(noticed.text.contains(
            "Onboarding was not completed. The onboarding fee of $600.00 will be charged on November 27, 2026."
        ));
        let none = onboarding_text(None);
        assert!(!none.text.contains("Onboarding"));
        assert!(!none.html.contains("Onboarding"));
    }

    #[test]
    fn dates_and_times_are_local_to_the_signup_zone() {
        assert_eq!(
            format_local_date(at("2026-11-21 03:00:00")),
            "November 20, 2026"
        );
        assert_eq!(
            format_local_date(at("2026-11-21 05:00:00")),
            "November 21, 2026"
        );
        assert_eq!(
            format_local_time(at("2026-10-27 14:30:00")),
            "Tue, Oct 27 at 10:30 AM EDT"
        );
        assert_eq!(
            format_local_time(at("2026-11-05 01:00:00")),
            "Wed, Nov 4 at 8:00 PM EST"
        );
        let quote = FirstChargeQuote {
            charge_at: at("2026-10-09 02:00:00"),
            ..quote()
        };
        let email = render_first_charge_email(&FirstChargeEmailInput {
            company_name: "Rock & Co",
            user_name: None,
            quote: &quote,
            app_url: APP_URL,
            account_url: ACCOUNT_URL,
            onboarding: None,
        });
        assert_eq!(
            email.subject,
            "Your first Granite Manager charge is on October 8, 2026"
        );
        assert!(email.text.contains("To cancel before October 8, 2026"));
    }

    #[test]
    fn only_open_or_completed_commitments_get_a_line() {
        use crate::onboarding_call::commitment::{Booking, new_commitment};
        let base = new_commitment(
            7,
            at("2026-10-01 12:00:00"),
            at("2026-10-31 12:00:00"),
            60_000,
        )
        .commitment;
        let with = |status| Commitment {
            status,
            ..base.clone()
        };
        assert_eq!(
            OnboardingLine::of(&with(Status::Completed)),
            Some(OnboardingLine::Complete)
        );
        let call = Booking {
            invitee_uri: "i".into(),
            event_uri: "e".into(),
            start_at: at("2026-10-20 15:00:00"),
            end_at: at("2026-10-20 16:00:00"),
            reschedule_url: None,
            cancel_url: None,
            booked_at: at("2026-10-02 12:00:00"),
        };
        let booked = Commitment {
            booking: Some(call),
            ..with(Status::Booked)
        };
        assert_eq!(
            OnboardingLine::of(&booked),
            Some(OnboardingLine::Booked {
                start_at: Some(at("2026-10-20 15:00:00"))
            })
        );
        assert_eq!(
            OnboardingLine::of(&with(Status::Booked)),
            Some(OnboardingLine::Booked { start_at: None })
        );
        let not_booked = Some(OnboardingLine::NotBooked {
            deadline_at: at("2026-10-31 12:00:00"),
            fee_cents: 60_000,
        });
        assert_eq!(OnboardingLine::of(&with(Status::Pending)), not_booked);
        let noticed = Commitment {
            charge_after: Some(at("2026-11-07 12:00:00")),
            ..with(Status::NoticeSent)
        };
        assert_eq!(
            OnboardingLine::of(&noticed),
            Some(OnboardingLine::NoticeSent {
                charge_at: at("2026-11-07 12:00:00"),
                fee_cents: 60_000
            })
        );
        assert_eq!(OnboardingLine::of(&with(Status::NoticeSent)), not_booked);
        for status in [
            Status::Waived,
            Status::Suspended,
            Status::FeeInvoiced,
            Status::FeePaid,
            Status::FeeFailed,
            Status::FeeSkipped,
            Status::Void,
        ] {
            assert_eq!(OnboardingLine::of(&with(status)), None);
        }
    }

    #[test]
    fn money_has_separators_and_two_decimals() {
        assert_eq!(format_money(0), "$0.00");
        assert_eq!(format_money(5), "$0.05");
        assert_eq!(format_money(3000), "$30.00");
        assert_eq!(format_money(123_450), "$1,234.50");
        assert_eq!(format_money(100_000_000), "$1,000,000.00");
    }
}
