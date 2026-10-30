//! The payment method emails (step 13).
//!
//! A hold on the saved method (blocked bank account, dispute) and a bank
//! account that could not be verified, to the company's Managers, and the
//! internal notice to `BILLING_ALERT_EMAIL`.
//!
//! Pure: subjects, escaped HTML and text parts. The internal notice names the
//! company (id and name) only: no customer emails, account or card numbers.

use super::MethodHold;
use super::reminder_emails::{
    RenderedEmail, SUPPORT_EMAIL, company_or_default, escape_html, footer, format_local_date,
    format_money, greeting, html_button, html_footer, html_paragraph, wrap_html,
};
use chrono::NaiveDateTime;

pub const BLOCKED_SUBJECT: &str = "Your bank account can't be used anymore";
pub const DISPUTED_SUBJECT: &str = "A payment was disputed";
pub const BANK_FAILED_SUBJECT: &str = "We couldn't confirm your bank account";
const UPDATE_LABEL: &str = "Update payment method";
const ADD_LABEL: &str = "Add a payment method";
const ACCOUNT_LABEL: &str = "Open your account page";

/// The disputed payment behind a dispute hold, when Stripe gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisputedPayment {
    pub amount_cents: i64,
    /// When the payment was made.
    pub paid_at: NaiveDateTime,
}

/// One Manager's copy of a hold notice.
#[derive(Debug, Clone, Copy)]
pub struct HoldInput<'a> {
    pub hold: MethodHold,
    /// The disputed payment (a dispute hold only).
    pub disputed: Option<DisputedPayment>,
    pub company_name: &'a str,
    pub user_name: Option<&'a str>,
    /// When access pauses: unless a new method is saved (a blocked
    /// account), or until the dispute is resolved (a dispute).
    pub pauses_at: NaiveDateTime,
    /// The payments app's account page.
    pub account_url: &'a str,
}

/// Which bank verification failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BankFailure {
    /// The signup's bank account: access ends at this time unless another
    /// method is added.
    Signup(NaiveDateTime),
    /// A bank account added on the account page: the current method stays.
    Replacement,
}

/// One Manager's copy of the "could not verify" notice.
#[derive(Debug, Clone, Copy)]
pub struct BankFailedInput<'a> {
    pub failure: BankFailure,
    pub company_name: &'a str,
    pub user_name: Option<&'a str>,
    pub account_url: &'a str,
}

/// What staff hear about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalEvent<'a> {
    /// A bank debit came back (`charge.failed`): Stripe's failure code.
    AchReturn {
        failure_code: Option<&'a str>,
        amount_cents: Option<i64>,
    },
    /// The saved bank account was blocked: Stripe's reason.
    Blocked {
        reason: Option<&'a str>,
        hold: HoldOutcome,
    },
    /// A payment was disputed: Stripe's reason.
    Dispute {
        reason: Option<&'a str>,
        amount_cents: Option<i64>,
        hold: HoldOutcome,
    },
    /// A dispute was closed (`won`, `lost`, `warning_closed`).
    DisputeClosed {
        status: Option<&'a str>,
        amount_cents: Option<i64>,
        /// The company's dispute hold was lifted (it won and no other
        /// dispute holds it).
        hold_cleared: bool,
    },
    /// A superuser lifted the company's payment method hold.
    HoldCleared {
        /// The superuser's user id.
        by_user_id: i32,
        /// The hold that was lifted (`blocked`, `disputed`), `None` when none
        /// ran (or a new dispute keeps it).
        cleared: Option<&'a str>,
        /// Open or lost disputes marked resolved by staff.
        disputes_resolved: u64,
    },
}

/// What an event did to the company's access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldOutcome {
    /// A new hold: access pauses in `HOLD_DAYS` unless a new method is saved
    /// (a blocked account) or the dispute is won.
    Set,
    /// A hold runs already (or, for a blocked account, the method was
    /// replaced since).
    Unchanged,
    /// A legacy company: never held, staff follow up.
    Legacy,
    /// No company has the Stripe customer: nothing held.
    NoCompany,
}

impl HoldOutcome {
    const fn access(self) -> &'static str {
        match self {
            Self::Set => "pauses in 7 days unless a new method is saved",
            Self::Unchanged => "unchanged (a hold runs already, or the method was replaced)",
            Self::Legacy => "not paused (legacy plan): follow up with the company",
            Self::NoCompany => {
                "nothing paused (no company matches): look the customer up in Stripe"
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct InternalNotice<'a> {
    pub event: InternalEvent<'a>,
    /// `None`: no company has the Stripe customer (`stripe_customer`).
    pub company_id: Option<i32>,
    pub company_name: &'a str,
    /// The Stripe customer, named when no company matches it.
    pub stripe_customer: Option<&'a str>,
}

fn render(
    subject: &str,
    user_name: Option<&str>,
    company: &str,
    paragraphs: &[String],
    link: &str,
    action: &str,
) -> RenderedEmail {
    let company = company_or_default(company);
    let hello = greeting(user_name);
    let mut parts = vec![html_paragraph(&escape_html(&hello))];
    parts.extend(paragraphs.iter().map(|p| html_paragraph(&escape_html(p))));
    parts.push(html_button(link, action));
    parts.push(html_footer(&escape_html(company)));
    let mut text = vec![hello];
    text.extend(paragraphs.iter().cloned());
    text.push(format!("{action}: {link}"));
    text.push(footer(company));
    RenderedEmail {
        subject: subject.to_string(),
        html: wrap_html(parts),
        text: text.join("\n\n"),
    }
}

/// The hold notice (D10 blocked, D11 disputed), for one Manager.
pub fn render_hold(input: &HoldInput<'_>) -> RenderedEmail {
    let date = format_local_date(input.pauses_at);
    let (subject, paragraphs) = match input.hold {
        MethodHold::Blocked => (
            BLOCKED_SUBJECT,
            vec![format!(
                "Your bank blocked payments from this account. Add another payment method by {date} to keep access."
            )],
        ),
        MethodHold::Disputed => {
            let what = input.disputed.map_or_else(
                || "A payment was disputed with your card issuer or bank.".to_string(),
                |payment| {
                    format!(
                        "Your payment of {} on {} was disputed with your card issuer or bank.",
                        format_money(payment.amount_cents),
                        format_local_date(payment.paid_at)
                    )
                },
            );
            (
                DISPUTED_SUBJECT,
                vec![
                    what,
                    format!(
                        "Access pauses on {date} until the dispute is closed in your favor. Email {SUPPORT_EMAIL} to resolve it sooner."
                    ),
                ],
            )
        }
    };
    // A new payment method does not lift a dispute hold.
    let action = match input.hold {
        MethodHold::Blocked => UPDATE_LABEL,
        MethodHold::Disputed => ACCOUNT_LABEL,
    };
    render(
        subject,
        input.user_name,
        input.company_name,
        &paragraphs,
        input.account_url,
        action,
    )
}

/// The bank account could not be verified (`setup_intent.setup_failed`),
/// for one Manager.
pub fn render_bank_failed(input: &BankFailedInput<'_>) -> RenderedEmail {
    let (paragraphs, action) = match input.failure {
        BankFailure::Signup(until) => (
            vec![format!(
                "We couldn't verify your bank account. Add a card or another bank account by {} to keep using Granite Manager.",
                format_local_date(until)
            )],
            ADD_LABEL,
        ),
        BankFailure::Replacement => (
            vec![
                "We couldn't verify the bank account you added. Your current payment method stays in use.".to_string(),
                "To switch, add a card or another bank account on your account page.".to_string(),
            ],
            ACCOUNT_LABEL,
        ),
    };
    render(
        BANK_FAILED_SUBJECT,
        input.user_name,
        input.company_name,
        &paragraphs,
        input.account_url,
        action,
    )
}

impl InternalNotice<'_> {
    const fn title(&self) -> &'static str {
        match self.event {
            InternalEvent::AchReturn { .. } => "Bank debit returned",
            InternalEvent::Blocked { .. } => "Bank account blocked",
            InternalEvent::Dispute { .. } => "Payment disputed",
            InternalEvent::DisputeClosed { .. } => "Dispute closed",
            InternalEvent::HoldCleared { .. } => "Payment hold lifted by staff",
        }
    }

    fn rows(&self) -> Vec<(&'static str, String)> {
        let money = |cents: Option<i64>| cents.map_or_else(|| "unknown".to_string(), format_money);
        let company = self.company_id.map_or_else(
            || {
                format!(
                    "none matches Stripe customer {}",
                    self.stripe_customer.unwrap_or("unknown")
                )
            },
            |id| format!("{} (id {id})", company_or_default(self.company_name)),
        );
        let mut rows = vec![("Company", company)];
        match self.event {
            InternalEvent::AchReturn {
                failure_code,
                amount_cents,
            } => {
                rows.push(("Failure code", failure_code.unwrap_or("none").to_string()));
                rows.push(("Amount", money(amount_cents)));
            }
            InternalEvent::Blocked { reason, hold } => {
                rows.push(("Reason", reason.unwrap_or("none given").to_string()));
                rows.push(("Access", hold.access().into()));
            }
            InternalEvent::Dispute {
                reason,
                amount_cents,
                hold,
            } => {
                rows.push(("Reason", reason.unwrap_or("none given").to_string()));
                rows.push(("Amount", money(amount_cents)));
                let access = match hold {
                    HoldOutcome::Set => {
                        "pauses in 7 days and stays paused until the dispute is won or staff lift the hold"
                    }
                    HoldOutcome::Unchanged => "unchanged (a hold runs already)",
                    HoldOutcome::Legacy | HoldOutcome::NoCompany => hold.access(),
                };
                rows.push(("Access", access.into()));
            }
            InternalEvent::DisputeClosed {
                status,
                amount_cents,
                hold_cleared,
            } => {
                rows.push(("Outcome", status.unwrap_or("unknown").to_string()));
                rows.push(("Amount", money(amount_cents)));
                let access = if hold_cleared {
                    "its hold was lifted"
                } else {
                    "unchanged"
                };
                rows.push(("Access", access.into()));
            }
            InternalEvent::HoldCleared {
                by_user_id,
                cleared,
                disputes_resolved,
            } => {
                rows.push(("By", format!("superuser id {by_user_id}")));
                let hold = cleared.map_or_else(
                    || "none lifted (no hold ran, or a new dispute keeps it)".to_string(),
                    |hold| format!("{hold} hold lifted"),
                );
                rows.push(("Hold", hold));
                rows.push(("Disputes resolved", disputes_resolved.to_string()));
            }
        }
        rows
    }
}

/// The internal notice to `BILLING_ALERT_EMAIL`.
pub fn render_internal_notice(input: &InternalNotice<'_>) -> RenderedEmail {
    let rows = input.rows();
    let mut parts = vec![html_paragraph(&format!(
        "<strong>{}</strong>",
        input.title()
    ))];
    parts.extend(
        rows.iter()
            .map(|(label, value)| html_paragraph(&format!("{label}: {}", escape_html(value)))),
    );
    let mut text = vec![input.title().to_string()];
    text.extend(rows.iter().map(|(label, value)| format!("{label}: {value}")));
    RenderedEmail {
        subject: format!(
            "Billing alert: {}: {}",
            input.title(),
            if input.company_id.is_some() {
                company_or_default(input.company_name)
            } else {
                "no company"
            }
        ),
        html: wrap_html(parts),
        text: text.join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::test_store::at;

    const ACCOUNT: &str = "https://payments.example.com/account";

    #[test]
    fn hold_emails_say_why_and_until_when() {
        let blocked = render_hold(&HoldInput {
            hold: MethodHold::Blocked,
            disputed: None,
            company_name: "Acme <Stone>",
            user_name: Some("Dana"),
            pauses_at: at("2026-11-05 15:00:00"),
            account_url: ACCOUNT,
        });
        assert_eq!(blocked.subject, BLOCKED_SUBJECT);
        assert!(blocked.text.contains("Your bank blocked payments from this account. Add another payment method by November 5, 2026 to keep access."));
        assert!(blocked.text.contains("Update payment method: https://payments.example.com/account"));
        assert!(blocked.html.contains("Acme &lt;Stone&gt;"));
        let unknown = HoldInput {
            hold: MethodHold::Disputed,
            disputed: None,
            company_name: "Acme",
            user_name: None,
            pauses_at: at("2026-11-05 15:00:00"),
            account_url: ACCOUNT,
        };
        let disputed = render_hold(&HoldInput {
            disputed: Some(DisputedPayment {
                amount_cents: 37_500,
                paid_at: at("2026-10-01 15:00:00"),
            }),
            ..unknown
        });
        assert_eq!(disputed.subject, DISPUTED_SUBJECT);
        assert_eq!(
            disputed.text,
            "Hello,\n\nYour payment of $375.00 on October 1, 2026 was disputed with your card issuer or bank.\n\n\
             Access pauses on November 5, 2026 until the dispute is closed in your favor. Email support@granite-manager.com to resolve it sooner.\n\n\
             Open your account page: https://payments.example.com/account\n\n"
                .to_string()
                + &footer("Acme")
        );
        let without = render_hold(&unknown);
        assert!(without.text.contains("A payment was disputed with your card issuer or bank."));
        for email in [&blocked, &disputed, &without] {
            assert!(!email.text.contains('\u{2014}') && !email.html.contains('\u{2014}'));
        }
    }

    #[test]
    fn bank_failures_differ_for_a_signup_and_a_replacement() {
        let input = |failure| BankFailedInput {
            failure,
            company_name: "Acme",
            user_name: None,
            account_url: ACCOUNT,
        };
        let signup = render_bank_failed(&input(BankFailure::Signup(at("2026-11-06 15:00:00"))));
        assert!(signup.text.contains("by November 6, 2026 to keep using Granite Manager"));
        assert!(signup.text.contains("Add a payment method: "));
        let replacement = render_bank_failed(&input(BankFailure::Replacement));
        assert!(replacement.text.contains("Your current payment method stays in use."));
    }

    #[test]
    fn the_internal_notice_names_the_company_and_codes_only() {
        let notice = render_internal_notice(&InternalNotice {
            event: InternalEvent::AchReturn {
                failure_code: Some("insufficient_funds"),
                amount_cents: Some(37_500),
            },
            company_id: Some(20750),
            company_name: "Acme",
            stripe_customer: None,
        });
        assert_eq!(notice.subject, "Billing alert: Bank debit returned: Acme");
        assert!(notice.text.contains("Company: Acme (id 20750)"));
        assert!(notice.text.contains("Failure code: insufficient_funds"));
        assert!(notice.text.contains("Amount: $375.00"));
        let dispute = render_internal_notice(&InternalNotice {
            event: InternalEvent::Dispute {
                reason: Some("fraudulent"),
                amount_cents: None,
                hold: HoldOutcome::Legacy,
            },
            company_id: Some(1),
            company_name: "",
            stripe_customer: None,
        });
        assert!(dispute.text.contains("Reason: fraudulent"));
        assert!(dispute.text.contains("your company (id 1)"));
        assert!(dispute.text.contains("Access: not paused (legacy plan)"));
        let closed = render_internal_notice(&InternalNotice {
            event: InternalEvent::DisputeClosed {
                status: Some("won"),
                amount_cents: Some(37_500),
                hold_cleared: true,
            },
            company_id: Some(1),
            company_name: "Acme",
            stripe_customer: None,
        });
        assert_eq!(closed.subject, "Billing alert: Dispute closed: Acme");
        assert!(
            closed.text.contains("Outcome: won") && closed.text.contains("its hold was lifted")
        );
        let unmatched = render_internal_notice(&InternalNotice {
            event: InternalEvent::Dispute {
                reason: None,
                amount_cents: Some(100),
                hold: HoldOutcome::Unchanged,
            },
            company_id: None,
            company_name: "",
            stripe_customer: Some("cus_gone"),
        });
        assert_eq!(
            unmatched.subject,
            "Billing alert: Payment disputed: no company"
        );
        assert!(
            unmatched
                .text
                .contains("Company: none matches Stripe customer cus_gone")
        );
        let lifted = render_internal_notice(&InternalNotice {
            event: InternalEvent::HoldCleared {
                by_user_id: 7,
                cleared: Some("disputed"),
                disputes_resolved: 2,
            },
            company_id: Some(1),
            company_name: "Acme",
            stripe_customer: None,
        });
        assert_eq!(
            lifted.subject,
            "Billing alert: Payment hold lifted by staff: Acme"
        );
        assert!(lifted.text.contains("By: superuser id 7"));
        assert!(lifted.text.contains("Hold: disputed hold lifted"));
        assert!(lifted.text.contains("Disputes resolved: 2"));
    }
}
