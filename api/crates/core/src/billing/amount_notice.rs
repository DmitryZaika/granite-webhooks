//! The amount-change notice.
//!
//! When a seat or add-on change moves a self-serve
//! company's monthly total, its Managers get an email with the old and new
//! totals and the charge they first apply to, at most one a day (Terms 3.2,
//! 5 and the ACH authorization promise it before the changed charge).
//!
//! `company_billing.notified_monthly_cents` is the total the Managers were
//! last told about. The first time a company is seen it is set without an
//! email (every existing and newly provisioned company). When the total
//! differs and no notice went out today (`amount_notice_on`, a day in
//! [`super::catalog::SIGNUP_TZ`]), one guarded UPDATE claims the day and
//! moves the total (the run that changes the row sends), then every Manager
//! is emailed. While a scheduled cancellation ends the subscription before
//! the new total applies, the company waits untouched (the selection skips
//! it, so it never fills a run); withdrawing the cancellation lets the
//! change be announced.
//!
//! Each Manager's send is a row of `billing_reminder_emails`: cycle_kind
//! `amount_change`, cycle_started_at the claimed day (one claim a company a
//! day, so the day names the notice), step `from_<cents>` (the old total the
//! email names), inserted as `sending` before the email goes out. A failed
//! send stays `failed`, and the claimed day stands: the next day the company
//! is taken again and only the Managers whose send failed get the notice,
//! with the then-current total, up to [`REMINDER_MAX_ATTEMPTS`] days. If the
//! total moved again meanwhile, the new notice goes to every Manager, a
//! failed one's from the total they last heard. A send whose outcome is
//! unknown (stuck in `sending`) is never resent. Run by `POST
//! /v1/jobs/billing-reminders` after the reminders, at most
//! [`NOTICES_PER_TICK`] companies a run.

use super::MANAGER_POSITION;
use super::cancellation::{company_name, failure_kind, next_charge, send_rendered};
use super::catalog::signup_day;
use super::payments_account_url;
use super::reminder_emails::{
    RenderedEmail, SUPPORT_EMAIL, company_or_default, escape_html, footer, format_money, greeting,
    html_button, html_footer, html_paragraph, wrap_html,
};
use super::reminders::{REMINDER_MAX_ATTEMPTS, STUCK_SENDING_MINUTES};
use crate::state::AppState;
use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use lambda_http::tracing;
use sqlx::MySqlPool;
use std::collections::HashSet;
use std::time::Instant;

/// Companies looked at per run.
pub const NOTICES_PER_TICK: u32 = 50;
/// `billing_reminder_emails.cycle_kind` of the notices.
pub const CYCLE_KIND: &str = "amount_change";
/// `billing_reminder_emails.step` prefix; the old total (cents) follows.
const STEP_PREFIX: &str = "from_";
const ACCOUNT_LABEL: &str = "Open your account page";

/// What one company needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeDecision {
    /// First sight: remember the total, no email.
    Baseline,
    /// The Managers know the current total.
    Unchanged,
    /// A notice already went out today: tomorrow.
    NotToday,
    /// Changed, but the subscription ends before the new total applies: wait,
    /// the total untouched, until the cancellation is withdrawn.
    Quiet,
    /// Email the Managers.
    Notify { previous_cents: i64 },
    /// Unchanged since the last notice, whose send to some Managers failed:
    /// email those again.
    Retry,
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
    /// Failed sends of an earlier notice wait for a retry.
    pub retry_due: bool,
}

/// Baseline, wait or send, for `today` (a day in [`super::catalog::SIGNUP_TZ`]).
pub fn decide(facts: &NoticeFacts, today: NaiveDate) -> NoticeDecision {
    let Some(previous_cents) = facts.notified_cents else {
        return NoticeDecision::Baseline;
    };
    let changed = previous_cents != facts.monthly_cents;
    if !changed && !facts.retry_due {
        return NoticeDecision::Unchanged;
    }
    if facts.notice_on.is_some_and(|on| on >= today) {
        return NoticeDecision::NotToday;
    }
    if let (Some(applies_at), Some(cancel_at)) = (facts.applies_at, facts.cancel_at)
        && applies_at >= cancel_at
    {
        return NoticeDecision::Quiet;
    }
    if changed {
        NoticeDecision::Notify { previous_cents }
    } else {
        NoticeDecision::Retry
    }
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
    retry_due: i64,
}

/// Self-serve companies with a running subscription whose total was never
/// seen, or that changed (or has failed sends to retry) with no notice
/// today. A company whose scheduled cancellation ends the subscription before
/// the new total applies is left out here, so waiting ones never fill a run.
async fn candidates(state: &AppState, today: NaiveDate) -> sqlx::Result<Vec<CandidateRow>> {
    let retry_due = format!(
        "EXISTS (SELECT 1 FROM billing_reminder_emails r
                  WHERE r.company_id = b.company_id AND r.cycle_kind = '{CYCLE_KIND}'
                    AND r.status = 'failed' AND r.attempts < {REMINDER_MAX_ATTEMPTS})"
    );
    let applies_at = "CASE WHEN b.subscription_status = 'trialing'
                           THEN COALESCE(b.trial_end_at, b.current_period_end)
                           ELSE b.current_period_end END";
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT b.company_id, b.subscription_status,
                CAST(b.monthly_price_cents AS SIGNED) AS monthly_price_cents,
                CAST(b.notified_monthly_cents AS SIGNED) AS notified_monthly_cents,
                b.amount_notice_on, b.trial_end_at, b.current_period_end, b.cancel_at,
                CAST({retry_due} AS SIGNED) AS retry_due
           FROM company_billing b JOIN company c ON c.id = b.company_id
          WHERE c.plan_source = 'subscription'
            AND b.subscription_status IN ('trialing', 'active', 'past_due')
            AND b.monthly_price_cents IS NOT NULL
            AND (b.notified_monthly_cents IS NULL
                 OR ((b.notified_monthly_cents <> b.monthly_price_cents OR {retry_due})
                     AND (b.amount_notice_on IS NULL OR b.amount_notice_on < ?)
                     AND (b.cancel_at IS NULL OR ({applies_at}) IS NULL
                          OR b.cancel_at > ({applies_at}))))
          ORDER BY b.company_id
          LIMIT ?"
    )))
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
    if let Err(err) = sweep_stuck(&state.pool, now).await {
        tracing::warn!(error = %err, "amount notice sweep failed");
    }
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
        match notice_company(state, &row, now, today).await {
            Ok(true) => notified += 1,
            Ok(false) => {}
            Err(err) => tracing::warn!(company_id, error = %err, "amount notice failed"),
        }
    }
    notified
}

/// What every Manager's email of one company's run shares.
struct Notice {
    company_id: i32,
    company: String,
    account_url: String,
    current_cents: i64,
    applies_at: Option<NaiveDateTime>,
    prorated: bool,
    /// `cycle_started_at` of the sends claimed now: the claimed day.
    day: NaiveDateTime,
    now: NaiveDateTime,
}

/// One company; true when some Manager was emailed.
async fn notice_company(
    state: &AppState,
    row: &CandidateRow,
    now: NaiveDateTime,
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
        retry_due: row.retry_due != 0,
    };
    let previous_cents = match decide(&facts, today) {
        NoticeDecision::Unchanged | NoticeDecision::NotToday | NoticeDecision::Quiet => {
            return Ok(false);
        }
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
        NoticeDecision::Notify { previous_cents } => Some(previous_cents),
        NoticeDecision::Retry => None,
    };
    // Read before the day is claimed: a failed read leaves it to the next run.
    let managers = company_managers(pool, company_id).await?;
    let waiting = waiting_sends(pool, company_id).await?;
    let claimed = sqlx::query(
        "UPDATE company_billing SET notified_monthly_cents = ?, amount_notice_on = ?
          WHERE company_id = ? AND notified_monthly_cents = ? AND monthly_price_cents = ?
            AND (amount_notice_on IS NULL OR amount_notice_on < ?)",
    )
    .bind(row.monthly_price_cents)
    .bind(today)
    .bind(company_id)
    .bind(previous_cents.unwrap_or(row.monthly_price_cents))
    .bind(row.monthly_price_cents)
    .bind(today)
    .execute(pool)
    .await?;
    if claimed.rows_affected() != 1 {
        return Ok(false);
    }
    let notice = Notice {
        company_id,
        company: company_name(pool, company_id).await,
        account_url: payments_account_url(),
        current_cents: row.monthly_price_cents,
        applies_at,
        prorated: row.subscription_status != "trialing",
        day: today.and_time(NaiveTime::MIN),
        now,
    };
    let mut sent = 0;
    let mut retaken = HashSet::new();
    for manager in &managers {
        let earlier = waiting.iter().find(|w| w.user_id == manager.user_id);
        // A Manager whose earlier send failed is told from the total they
        // last heard; a retry is only for those.
        let from_cents = match (previous_cents, earlier) {
            (_, Some(earlier)) => earlier.from_cents,
            (Some(previous), None) => previous,
            (None, None) => continue,
        };
        if from_cents == notice.current_cents {
            continue;
        }
        let retake = match (previous_cents, earlier) {
            (None, Some(earlier)) => {
                retaken.insert(earlier.id);
                Some(earlier.id)
            }
            _ => None,
        };
        match send_one(state, &notice, manager, from_cents, retake).await {
            Ok(true) => sent += 1,
            Ok(false) => {}
            Err(err) => tracing::warn!(company_id, error = %err, "amount notice send not recorded"),
        }
    }
    // Every other waiting send is over: this notice covers it, the total is
    // back to what that Manager heard, or they are no longer a Manager.
    for earlier in waiting.iter().filter(|w| !retaken.contains(&w.id)) {
        if let Err(err) = close_send(pool, company_id, earlier.id).await {
            tracing::warn!(company_id, error = %err, "amount notice send not closed");
        }
    }
    Ok(sent > 0)
}

/// Claims, sends and records one Manager's notice: a new send of today's
/// cycle, or `retake` (a failed send of an earlier day) again. True when the
/// email went out.
async fn send_one(
    state: &AppState,
    notice: &Notice,
    manager: &Manager,
    from_cents: i64,
    retake: Option<i64>,
) -> sqlx::Result<bool> {
    let pool = &state.pool;
    let company_id = notice.company_id;
    let id = match retake {
        Some(id) => retake_send(pool, company_id, id, notice.now)
            .await?
            .then_some(id),
        None => {
            claim_send(
                pool,
                company_id,
                manager.user_id,
                notice.day,
                from_cents,
                notice.now,
            )
            .await?
        }
    };
    let Some(id) = id else {
        return Ok(false);
    };
    let email = render_amount_notice(&AmountNoticeInput {
        company_name: &notice.company,
        user_name: manager.name.as_deref(),
        previous_cents: from_cents,
        current_cents: notice.current_cents,
        applies_at: notice.applies_at,
        prorated: notice.prorated,
        account_url: &notice.account_url,
    });
    let result = send_rendered(state, &manager.email, &email).await;
    let (status, error_code, sent_at) = match &result {
        Ok(..) => ("sent", None, Some(notice.now)),
        Err(err) => {
            tracing::warn!(
                company_id,
                kind = failure_kind(err),
                "amount notice email failed"
            );
            ("failed", Some(failure_kind(err)), None)
        }
    };
    mark_send(pool, company_id, id, status, error_code, sent_at).await?;
    Ok(result.is_ok())
}

/// A Manager to email, with the user id their send is recorded under.
#[derive(Debug)]
struct Manager {
    user_id: i32,
    name: Option<String>,
    email: String,
}

/// The company's Managers (position 5 in the company, not deleted) with an
/// email, one per address, as `cancellation::manager_contacts` reads them.
async fn company_managers(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<Manager>> {
    let rows: Vec<(i32, Option<String>, String)> = sqlx::query_as(
        "SELECT u.id, u.name, TRIM(u.email) FROM users u
          WHERE u.company_id = ? AND u.is_deleted = 0
            AND u.email IS NOT NULL AND TRIM(u.email) <> ''
            AND EXISTS (SELECT 1 FROM users_positions up
                         WHERE up.user_id = u.id AND up.company_id = u.company_id
                           AND up.position_id = ?)
          ORDER BY u.id",
    )
    .bind(company_id)
    .bind(MANAGER_POSITION)
    .fetch_all(pool)
    .await?;
    let mut seen = HashSet::new();
    Ok(rows
        .into_iter()
        .filter(|(_, _, email)| seen.insert(email.to_lowercase()))
        .map(|(user_id, name, email)| Manager {
            user_id,
            name,
            email,
        })
        .collect())
}

/// A failed send that may be retried.
#[derive(Debug)]
struct Waiting {
    id: i64,
    user_id: i32,
    /// The old total its email named.
    from_cents: i64,
}

fn step_for(from_cents: i64) -> String {
    format!("{STEP_PREFIX}{from_cents}")
}

fn from_step(step: &str) -> Option<i64> {
    step.strip_prefix(STEP_PREFIX)?.parse().ok()
}

/// The company's failed sends with attempts left, oldest first.
async fn waiting_sends(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<Waiting>> {
    let rows: Vec<(i64, i32, String)> = sqlx::query_as(
        "SELECT id, user_id, step FROM billing_reminder_emails
          WHERE company_id = ? AND cycle_kind = ? AND status = 'failed' AND attempts < ?
          ORDER BY cycle_started_at, id",
    )
    .bind(company_id)
    .bind(CYCLE_KIND)
    .bind(REMINDER_MAX_ATTEMPTS)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, user_id, step)| {
            Some(Waiting {
                id,
                user_id,
                from_cents: from_step(&step)?,
            })
        })
        .collect())
}

fn is_duplicate(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
}

/// Inserts a Manager's send of the notice claimed on `day` as `sending`,
/// before the email goes out. `None` when it already exists.
async fn claim_send(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    day: NaiveDateTime,
    from_cents: i64,
    now: NaiveDateTime,
) -> sqlx::Result<Option<i64>> {
    let inserted = sqlx::query(
        "INSERT INTO billing_reminder_emails
           (company_id, user_id, cycle_kind, cycle_started_at, step, status, attempts, claimed_at)
         VALUES (?, ?, ?, ?, ?, 'sending', 1, ?)",
    )
    .bind(company_id)
    .bind(user_id)
    .bind(CYCLE_KIND)
    .bind(day)
    .bind(step_for(from_cents))
    .bind(now)
    .execute(pool)
    .await;
    match inserted {
        Ok(done) => Ok(i64::try_from(done.last_insert_id()).ok()),
        Err(err) if is_duplicate(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

/// Takes a failed send again; false when another run took it or it is out
/// of attempts.
async fn retake_send(
    pool: &MySqlPool,
    company_id: i32,
    id: i64,
    now: NaiveDateTime,
) -> sqlx::Result<bool> {
    let retaken = sqlx::query(
        "UPDATE billing_reminder_emails
            SET status = 'sending', attempts = attempts + 1, claimed_at = ?, error_code = NULL
          WHERE id = ? AND company_id = ? AND cycle_kind = ? AND status = 'failed'
            AND attempts < ?",
    )
    .bind(now)
    .bind(id)
    .bind(company_id)
    .bind(CYCLE_KIND)
    .bind(REMINDER_MAX_ATTEMPTS)
    .execute(pool)
    .await?;
    Ok(retaken.rows_affected() == 1)
}

/// How a claimed send went: `sent` or `failed` (retried the next day).
async fn mark_send(
    pool: &MySqlPool,
    company_id: i32,
    id: i64,
    status: &str,
    error_code: Option<&str>,
    sent_at: Option<NaiveDateTime>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE billing_reminder_emails
            SET status = ?, error_code = ?, sent_at = COALESCE(?, sent_at)
          WHERE id = ? AND company_id = ? AND status = 'sending'",
    )
    .bind(status)
    .bind(error_code)
    .bind(sent_at)
    .bind(id)
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A failed send no retry is due for any more.
async fn close_send(pool: &MySqlPool, company_id: i32, id: i64) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE billing_reminder_emails SET status = 'skipped', error_code = 'superseded'
          WHERE id = ? AND company_id = ? AND status = 'failed'",
    )
    .bind(id)
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Sends still `sending` after 15 minutes (a crash between the send and its
/// record) become `unknown`, never resent.
async fn sweep_stuck(pool: &MySqlPool, now: NaiveDateTime) -> sqlx::Result<u64> {
    let swept = sqlx::query(
        "UPDATE billing_reminder_emails
            SET status = 'unknown', error_code = 'stuck_sending'
          WHERE cycle_kind = ? AND status = 'sending' AND claimed_at < ?",
    )
    .bind(CYCLE_KIND)
    .bind(now - Duration::minutes(STUCK_SENDING_MINUTES))
    .execute(pool)
    .await?;
    Ok(swept.rows_affected())
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
            retry_due: false,
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
    fn a_change_waits_quietly_while_the_subscription_ends_first() {
        let today = day("2026-10-24");
        let ending = NoticeFacts {
            cancel_at: Some(at("2026-11-21 05:00:00")),
            ..facts(Some(33_000), None)
        };
        assert_eq!(decide(&ending, today), NoticeDecision::Quiet);
        assert_eq!(
            decide(
                &NoticeFacts {
                    retry_due: true,
                    monthly_cents: 33_000,
                    ..ending
                },
                today
            ),
            NoticeDecision::Quiet
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
        // The cancellation withdrawn: announced.
        let withdrawn = NoticeFacts {
            cancel_at: None,
            ..ending
        };
        assert_eq!(
            decide(&withdrawn, today),
            NoticeDecision::Notify {
                previous_cents: 33_000
            }
        );
    }

    #[test]
    fn failed_sends_are_retried_on_a_later_day() {
        let today = day("2026-10-25");
        let waiting = NoticeFacts {
            retry_due: true,
            ..facts(Some(36_000), Some("2026-10-24"))
        };
        assert_eq!(decide(&waiting, today), NoticeDecision::Retry);
        assert_eq!(
            decide(&waiting, day("2026-10-24")),
            NoticeDecision::NotToday
        );
        // Changed again meanwhile: a new notice, which covers the retry.
        assert_eq!(
            decide(
                &NoticeFacts {
                    notified_cents: Some(33_000),
                    ..waiting
                },
                today
            ),
            NoticeDecision::Notify {
                previous_cents: 33_000
            }
        );
    }

    #[test]
    fn the_step_carries_the_old_total() {
        assert_eq!(step_for(30_000), "from_30000");
        assert_eq!(from_step("from_30000"), Some(30_000));
        assert_eq!(from_step("started"), None);
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
