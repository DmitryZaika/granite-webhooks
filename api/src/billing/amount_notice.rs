//! The amount-change notice: when a seat or add-on change moves a self-serve
//! company's monthly total, its Managers get an email with the old and new
//! totals and the charge they first apply to, at most one a day (Terms 3.2,
//! 5 and the ACH authorization promise it before the changed charge).
//!
//! `company_billing.notified_monthly_cents` is the total the Managers were
//! last told about. The first time a company is seen it is set without an
//! email (every existing and newly provisioned company). When the total
//! differs and no notice went out today (`amount_notice_on`, a day in
//! [`super::catalog::SIGNUP_TZ`]), one guarded UPDATE claims the notice (the
//! run that changes the row sends it), then every Manager is emailed; if
//! every send fails the claim is given back so the next run retries. A company whose scheduled
//! cancellation ends the subscription before the new total applies is not
//! emailed, but its baseline still moves. Run by `POST
//! /v1/jobs/billing-reminders` after the reminders, at most
//! [`NOTICES_PER_TICK`] companies a run.

use super::cancellation::{
    company_name, failure_kind, manager_contacts, next_charge, send_rendered,
};
use super::catalog::signup_day;
use super::payments_account_url;
use super::reminder_emails::{
    RenderedEmail, SUPPORT_EMAIL, company_or_default, escape_html, footer, format_money, greeting,
    html_button, html_footer, html_paragraph, wrap_html,
};
use crate::state::AppState;
use chrono::{NaiveDate, NaiveDateTime};
use lambda_http::tracing;
use std::time::Instant;

/// Companies looked at per run.
pub const NOTICES_PER_TICK: u32 = 50;
const ACCOUNT_LABEL: &str = "Open your account page";

/// What one company needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeDecision {
    /// First sight: remember the total, no email.
    Baseline,
    /// The Managers know the current total.
    Unchanged,
    /// Changed, but a notice already went out today: tomorrow.
    NotToday,
    /// Changed, but the subscription ends before the new total applies:
    /// remember it, no email.
    Quiet { previous_cents: i64 },
    /// Email the Managers.
    Notify { previous_cents: i64 },
}

/// A company's totals and dates, as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoticeFacts {
    /// `monthly_price_cents`: the current monthly total before tax.
    pub monthly_cents: i64,
    /// `notified_monthly_cents`.
    pub notified_cents: Option<i64>,
    /// `amount_notice_on`.
    pub notice_on: Option<NaiveDate>,
    /// The charge the new total first applies to.
    pub applies_at: Option<NaiveDateTime>,
    /// When a scheduled cancellation ends the subscription.
    pub cancel_at: Option<NaiveDateTime>,
}

/// Baseline, wait or send, for `today` (a day in [`super::catalog::SIGNUP_TZ`]).
pub fn decide(facts: &NoticeFacts, today: NaiveDate) -> NoticeDecision {
    let Some(previous_cents) = facts.notified_cents else {
        return NoticeDecision::Baseline;
    };
    if previous_cents == facts.monthly_cents {
        return NoticeDecision::Unchanged;
    }
    if facts.notice_on.is_some_and(|on| on >= today) {
        return NoticeDecision::NotToday;
    }
    if let (Some(applies_at), Some(cancel_at)) = (facts.applies_at, facts.cancel_at)
        && applies_at >= cancel_at
    {
        return NoticeDecision::Quiet { previous_cents };
    }
    NoticeDecision::Notify { previous_cents }
}

/// One Manager's notice.
#[derive(Debug, Clone, Copy)]
pub struct AmountNoticeInput<'a> {
    pub company_name: &'a str,
    pub user_name: Option<&'a str>,
    pub previous_cents: i64,
    pub current_cents: i64,
    /// The charge the new total first applies to, when known.
    pub applies_at: Option<NaiveDateTime>,
    /// Changed after the free period: that invoice carries a prorated
    /// adjustment for the current period.
    pub prorated: bool,
    /// The payments app's account page.
    pub account_url: &'a str,
}

fn notice_paragraphs(input: &AmountNoticeInput<'_>, company: &str) -> Vec<String> {
    let old = format_money(input.previous_cents);
    let new = format_money(input.current_cents);
    let mut paragraphs = vec![format!(
        "The monthly total for {company} changed from {old} plus applicable sales tax to {new} plus applicable sales tax."
    )];
    paragraphs.push(input.applies_at.map_or_else(
        || "The new total first applies to your next charge.".to_string(),
        |at| {
            format!(
                "The new total first applies to the charge on {}.",
                super::reminder_emails::format_local_date(at)
            )
        },
    ));
    if input.prorated {
        paragraphs.push(
            "Because the change was made during a paid month, that invoice also shows a prorated adjustment for the rest of the current month.".to_string(),
        );
    }
    paragraphs.push(
        "Each active user above 10 is $30.00 per month. Your add-ons and their prices are listed on your account page.".to_string(),
    );
    paragraphs.push(format!(
        "Questions about this change? Reply to this email or write to {SUPPORT_EMAIL}."
    ));
    paragraphs
}

/// The amount-change email.
pub fn render_amount_notice(input: &AmountNoticeInput<'_>) -> RenderedEmail {
    let hello = greeting(input.user_name);
    let company = company_or_default(input.company_name);
    let company_html = escape_html(company);
    let mut parts = vec![html_paragraph(&escape_html(&hello))];
    parts.extend(
        notice_paragraphs(input, &company_html)
            .iter()
            .map(|p| html_paragraph(p)),
    );
    parts.push(html_button(input.account_url, ACCOUNT_LABEL));
    parts.push(html_footer(&company_html));
    let mut text = vec![hello];
    text.extend(notice_paragraphs(input, company));
    text.push(format!("{ACCOUNT_LABEL}: {}", input.account_url));
    text.push(footer(company));
    RenderedEmail {
        subject: format!(
            "Your Granite Manager monthly total is now {}",
            format_money(input.current_cents)
        ),
        html: wrap_html(parts),
        text: text.join("\n\n"),
    }
}

#[derive(Debug, sqlx::FromRow)]
struct CandidateRow {
    company_id: i32,
    subscription_status: String,
    monthly_price_cents: i64,
    notified_monthly_cents: Option<i64>,
    amount_notice_on: Option<NaiveDate>,
    trial_end_at: Option<NaiveDateTime>,
    current_period_end: Option<NaiveDateTime>,
    cancel_at: Option<NaiveDateTime>,
}

/// Self-serve companies with a running subscription whose total was never
/// seen, or changed with no notice today.
async fn candidates(state: &AppState, today: NaiveDate) -> sqlx::Result<Vec<CandidateRow>> {
    sqlx::query_as(
        "SELECT b.company_id, b.subscription_status,
                CAST(b.monthly_price_cents AS SIGNED) AS monthly_price_cents,
                CAST(b.notified_monthly_cents AS SIGNED) AS notified_monthly_cents,
                b.amount_notice_on, b.trial_end_at, b.current_period_end, b.cancel_at
           FROM company_billing b JOIN company c ON c.id = b.company_id
          WHERE c.plan_source = 'subscription'
            AND b.subscription_status IN ('trialing', 'active', 'past_due')
            AND b.monthly_price_cents IS NOT NULL
            AND (b.notified_monthly_cents IS NULL
                 OR (b.notified_monthly_cents <> b.monthly_price_cents
                     AND (b.amount_notice_on IS NULL OR b.amount_notice_on < ?)))
          ORDER BY b.company_id
          LIMIT ?",
    )
    .bind(today)
    .bind(NOTICES_PER_TICK)
    .fetch_all(&state.pool)
    .await
}

/// Sends the due amount-change notices as of `now` (UTC), starting no new
/// company after `deadline`. Answers how many companies' Managers were
/// emailed. Errors are logged with the company id only.
pub async fn process_amount_notices(
    state: &AppState,
    now: NaiveDateTime,
    deadline: Instant,
) -> usize {
    let today = signup_day(now.and_utc());
    let rows = match candidates(state, today).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, "amount notices not read");
            return 0;
        }
    };
    let mut notified = 0;
    for row in rows {
        if Instant::now() >= deadline {
            break;
        }
        let company_id = row.company_id;
        match notice_company(state, &row, today).await {
            Ok(true) => notified += 1,
            Ok(false) => {}
            Err(err) => tracing::warn!(company_id, error = %err, "amount notice failed"),
        }
    }
    notified
}

/// One company; true when its Managers were emailed.
async fn notice_company(
    state: &AppState,
    row: &CandidateRow,
    today: NaiveDate,
) -> sqlx::Result<bool> {
    let pool = &state.pool;
    let company_id = row.company_id;
    let applies_at = next_charge(
        &row.subscription_status,
        row.trial_end_at,
        row.current_period_end,
    );
    let facts = NoticeFacts {
        monthly_cents: row.monthly_price_cents,
        notified_cents: row.notified_monthly_cents,
        notice_on: row.amount_notice_on,
        applies_at,
        cancel_at: row.cancel_at,
    };
    let previous_cents = match decide(&facts, today) {
        NoticeDecision::Unchanged | NoticeDecision::NotToday => return Ok(false),
        NoticeDecision::Baseline => {
            sqlx::query(
                "UPDATE company_billing SET notified_monthly_cents = ?
                  WHERE company_id = ? AND notified_monthly_cents IS NULL",
            )
            .bind(row.monthly_price_cents)
            .bind(company_id)
            .execute(pool)
            .await?;
            return Ok(false);
        }
        NoticeDecision::Quiet { previous_cents } => {
            sqlx::query(
                "UPDATE company_billing SET notified_monthly_cents = ?
                  WHERE company_id = ? AND notified_monthly_cents = ?",
            )
            .bind(row.monthly_price_cents)
            .bind(company_id)
            .bind(previous_cents)
            .execute(pool)
            .await?;
            return Ok(false);
        }
        NoticeDecision::Notify { previous_cents } => previous_cents,
    };
    let claimed = sqlx::query(
        "UPDATE company_billing SET notified_monthly_cents = ?, amount_notice_on = ?
          WHERE company_id = ? AND notified_monthly_cents = ? AND monthly_price_cents = ?
            AND (amount_notice_on IS NULL OR amount_notice_on < ?)",
    )
    .bind(row.monthly_price_cents)
    .bind(today)
    .bind(company_id)
    .bind(previous_cents)
    .bind(row.monthly_price_cents)
    .bind(today)
    .execute(pool)
    .await?;
    if claimed.rows_affected() != 1 {
        return Ok(false);
    }
    let contacts = manager_contacts(pool, company_id).await;
    let contacts = match contacts {
        Ok(contacts) => contacts,
        Err(err) => {
            restore(state, row, today).await;
            return Err(err);
        }
    };
    let company = company_name(pool, company_id).await;
    let account_url = payments_account_url();
    let mut sent = 0;
    for contact in &contacts {
        let email = render_amount_notice(&AmountNoticeInput {
            company_name: &company,
            user_name: contact.name.as_deref(),
            previous_cents,
            current_cents: row.monthly_price_cents,
            applies_at,
            prorated: row.subscription_status != "trialing",
            account_url: &account_url,
        });
        match send_rendered(state, &contact.email, &email).await {
            Ok(()) => sent += 1,
            Err(err) => tracing::warn!(
                company_id,
                kind = failure_kind(&err),
                "amount notice email failed"
            ),
        }
    }
    if sent == 0 && !contacts.is_empty() {
        restore(state, row, today).await;
        return Ok(false);
    }
    Ok(sent > 0)
}

/// Gives a claim back after every send failed, so the next run retries.
async fn restore(state: &AppState, row: &CandidateRow, today: NaiveDate) {
    let restored = sqlx::query(
        "UPDATE company_billing SET notified_monthly_cents = ?, amount_notice_on = ?
          WHERE company_id = ? AND notified_monthly_cents = ? AND amount_notice_on = ?",
    )
    .bind(row.notified_monthly_cents)
    .bind(row.amount_notice_on)
    .bind(row.company_id)
    .bind(row.monthly_price_cents)
    .bind(today)
    .execute(&state.pool)
    .await;
    if restored.is_err() {
        tracing::warn!(
            company_id = row.company_id,
            "amount notice claim not restored"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::test_store::at;

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    fn facts(notified: Option<i64>, notice_on: Option<&str>) -> NoticeFacts {
        NoticeFacts {
            monthly_cents: 36_000,
            notified_cents: notified,
            notice_on: notice_on.map(day),
            applies_at: Some(at("2026-11-21 05:00:00")),
            cancel_at: None,
        }
    }

    #[test]
    fn the_first_sight_is_a_baseline_without_email() {
        let today = day("2026-10-24");
        assert_eq!(decide(&facts(None, None), today), NoticeDecision::Baseline);
        assert_eq!(
            decide(&facts(None, Some("2026-10-24")), today),
            NoticeDecision::Baseline
        );
    }

    #[test]
    fn a_change_is_sent_once_a_day() {
        let today = day("2026-10-24");
        assert_eq!(
            decide(&facts(Some(36_000), None), today),
            NoticeDecision::Unchanged
        );
        assert_eq!(
            decide(&facts(Some(33_000), None), today),
            NoticeDecision::Notify {
                previous_cents: 33_000
            }
        );
        assert_eq!(
            decide(&facts(Some(33_000), Some("2026-10-23")), today),
            NoticeDecision::Notify {
                previous_cents: 33_000
            }
        );
        assert_eq!(
            decide(&facts(Some(33_000), Some("2026-10-24")), today),
            NoticeDecision::NotToday
        );
    }

    #[test]
    fn a_subscription_ending_first_moves_the_baseline_quietly() {
        let today = day("2026-10-24");
        let ending = NoticeFacts {
            cancel_at: Some(at("2026-11-21 05:00:00")),
            ..facts(Some(33_000), None)
        };
        assert_eq!(
            decide(&ending, today),
            NoticeDecision::Quiet {
                previous_cents: 33_000
            }
        );
        let ending_later = NoticeFacts {
            cancel_at: Some(at("2026-12-21 05:00:00")),
            ..ending
        };
        assert_eq!(
            decide(&ending_later, today),
            NoticeDecision::Notify {
                previous_cents: 33_000
            }
        );
    }

    fn input(prorated: bool) -> AmountNoticeInput<'static> {
        AmountNoticeInput {
            company_name: "Rock & <Roll>",
            user_name: Some("Ann"),
            previous_cents: 30_000,
            current_cents: 37_500,
            applies_at: Some(at("2027-01-21 05:00:00")),
            prorated,
            account_url: "https://payments.example.com/account",
        }
    }

    #[test]
    fn the_notice_states_both_totals_the_date_and_the_rules() {
        let email = render_amount_notice(&input(true));
        assert_eq!(
            email.subject,
            "Your Granite Manager monthly total is now $375.00"
        );
        assert!(email.text.starts_with("Hi Ann,"));
        assert!(email.text.contains(
            "The monthly total for Rock & <Roll> changed from $300.00 plus applicable sales tax to $375.00 plus applicable sales tax."
        ));
        assert!(
            email
                .text
                .contains("The new total first applies to the charge on January 21, 2027.")
        );
        assert!(email.text.contains("prorated adjustment"));
        assert!(email.text.contains(
            "Each active user above 10 is $30.00 per month. Your add-ons and their prices are listed on your account page."
        ));
        assert!(
            email
                .text
                .contains("Open your account page: https://payments.example.com/account")
        );
        assert!(email.html.contains("Rock &amp; &lt;Roll&gt;"));
        assert!(!email.html.contains("<Roll>"));
        assert_eq!(email.html.matches("href=").count(), 1);
        let all = format!("{}\n{}\n{}", email.subject, email.html, email.text);
        assert!(!all.contains('\u{2014}') && !all.contains('\u{2013}'));
    }

    #[test]
    fn a_change_in_the_free_period_has_no_proration_and_an_unknown_date_is_the_next_charge() {
        let email = render_amount_notice(&AmountNoticeInput {
            applies_at: None,
            ..input(false)
        });
        assert!(!email.text.contains("prorated"));
        assert!(
            email
                .text
                .contains("The new total first applies to your next charge.")
        );
    }
}
