//! How many calls a user may start.
//!
//! 6 a minute (app calls, Call via my cell, test calls and cell code calls
//! together; refused and failed calls do not count) and the company's daily
//! limit of outgoing calls (any outcome).
//!
//! Both are counted from the database, so they hold across API containers:
//! [`take_call_slot`] runs in the transaction that then logs the call. It
//! locks the user's settings row first, so two calls of one user at once
//! count one after the other, and the call row is inserted before the
//! commit lets the next one count it.

use super::settings;
use chrono::{DateTime, Utc};
use sqlx::{MySql, Transaction};

/// Calls a user may start in a minute.
pub const CALLS_PER_MINUTE: i64 = 6;
/// The window of [`CALLS_PER_MINUTE`], in seconds.
const MINUTE_SECS: i64 = 60;

/// SQL: a call `pc` its user started (an outgoing call, or a test call of
/// our number to their phone) that did not fail: what takes a call slot. A
/// refused call, or one Telnyx did not place, leaves the slot free.
const STARTED_BY_USER: &str = "pc.status <> 'failed'
     AND (pc.direction = 'outbound' OR pc.other_number <=> pc.our_number)";

/// Why a call may not start yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// 6 calls in the last minute: seconds until one of them is older.
    PerMinute(u64),
    /// The company's daily limit is used up: seconds until the next day.
    Daily(u64),
}

impl Wait {
    /// Seconds to wait (at least 1).
    pub const fn secs(self) -> u64 {
        let secs = match self {
            Self::PerMinute(secs) | Self::Daily(secs) => secs,
        };
        if secs == 0 { 1 } else { secs }
    }

    /// Why, for the log.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::PerMinute(_) => "6 calls a minute",
            Self::Daily(_) => "the company's daily call limit",
        }
    }
}

/// The company's daily limit, for a call that counts against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Daily {
    /// Outgoing calls a user may start per day.
    pub limit: i32,
    /// When today started (company time zone).
    pub since: DateTime<Utc>,
    /// When tomorrow starts.
    pub until: DateTime<Utc>,
}

/// Seconds as a wait: negative or zero is one second.
fn wait_secs(secs: i64) -> u64 {
    u64::try_from(secs).unwrap_or(0).max(1)
}

/// Takes one of the user's call slots in `tx`.
///
/// The transaction inserts the call next and then commits. This locks the
/// user's settings row (making it when missing), then checks the user
/// started fewer than [`CALLS_PER_MINUTE`] calls in the last minute (a cell
/// code call counts too) and, with `daily`, fewer outgoing calls today than
/// the company's limit (any outcome). `Err` says how long to wait.
pub async fn take_call_slot(
    tx: &mut Transaction<'_, MySql>,
    company_id: i32,
    user_id: i32,
    daily: Option<&Daily>,
) -> sqlx::Result<Result<(), Wait>> {
    settings::ensure_row(tx, user_id, company_id).await?;
    let (calls, oldest_left): (i64, Option<i64>) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(COUNT(*) AS SIGNED),
                CAST(TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(3),
                                   MIN(pc.started_at) + INTERVAL {MINUTE_SECS} SECOND) AS SIGNED)
           FROM phone_calls pc
          WHERE pc.company_id = ? AND pc.user_id = ?
            AND pc.started_at > UTC_TIMESTAMP(3) - INTERVAL {MINUTE_SECS} SECOND
            AND {STARTED_BY_USER}"
    )))
    .bind(company_id)
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    // The cell code call of the last minute, if any: seconds until it is older.
    let code_left: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(),
                                   cell_code_sent_at + INTERVAL {MINUTE_SECS} SECOND) AS SIGNED)
           FROM phone_user_settings
          WHERE user_id = ? AND cell_code_sent_at > UTC_TIMESTAMP() - INTERVAL {MINUTE_SECS} SECOND
          FOR UPDATE"
    )))
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    let started = calls + i64::from(code_left.is_some());
    if started >= CALLS_PER_MINUTE {
        let soonest = [oldest_left, code_left]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(1);
        return Ok(Err(Wait::PerMinute(wait_secs(soonest))));
    }
    if let Some(daily) = daily {
        let today: i64 = sqlx::query_scalar(
            "SELECT CAST(COUNT(*) AS SIGNED) FROM phone_calls
              WHERE company_id = ? AND user_id = ? AND direction = 'outbound' AND started_at >= ?",
        )
        .bind(company_id)
        .bind(user_id)
        .bind(daily.since)
        .fetch_one(&mut **tx)
        .await?;
        if today >= i64::from(daily.limit) {
            let left = (daily.until - Utc::now()).num_seconds();
            return Ok(Err(Wait::Daily(wait_secs(left))));
        }
    }
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_are_at_least_a_second() {
        assert_eq!(Wait::PerMinute(0).secs(), 1);
        assert_eq!(Wait::Daily(3600).secs(), 3600);
        assert_eq!(wait_secs(-5), 1);
        assert_eq!(wait_secs(42), 42);
        assert_eq!(Wait::PerMinute(5).reason(), "6 calls a minute");
    }
}
