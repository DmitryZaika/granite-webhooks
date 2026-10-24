//! Verifying my cell for Call via my cell and the cell fallback: our server
//! calls the cell and reads out a 6-digit code (valid 10 minutes, 5 tries),
//! which the app sends back. Code calls are limited (one a minute, five a
//! day per user, and each takes one of the user's call slots), whatever
//! becomes of the codes, so the route cannot ring a number over and over.

use super::{bad_request, calling_setup, denied, rate_limited, telnyx_failed};
use crate::auth::device::PhoneCaller;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::limits;
use crate::phone::lines::{self, default_caller_id};
use crate::phone::numbers::{is_nanp, to_e164};
use crate::phone::settings::{self, PhoneSettings};
use crate::routes::phone::RateLimited;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::FromRow;
use utoipa::ToSchema;

/// Wrong codes allowed before the code is void.
const CODE_TRIES: i64 = 5;
/// At most one code call a minute.
const CODE_GAP_SECS: i64 = 60;
/// At most this many code calls a day (a 24-hour window from the first).
const CODES_PER_DAY: i64 = 5;
const DAY_SECS: i64 = 86_400;

/// Body of `POST /v1/phone/cell/verify`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CellVerify {
    /// My cell number, US or Canada (E.164 or 10 digits).
    pub cell_number: String,
}

/// Body of `POST /v1/phone/cell/confirm`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CellConfirm {
    /// The 6-digit code our call read out.
    pub code: String,
}

/// A random 6-digit code.
fn new_code() -> Result<String, getrandom::Error> {
    // The largest multiple of 1,000,000 below 2^32, so every code is as likely.
    const LIMIT: u32 = 4_294_000_000;
    loop {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes)?;
        let value = u32::from_le_bytes(bytes);
        if value < LIMIT {
            return Ok(format!("{:06}", value % 1_000_000));
        }
    }
}

/// `a == b`, in time that does not depend on where they differ.
fn same_code(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |diff, (x, y)| diff | (x ^ y))
            == 0
}

/// The user's code calls so far, as the limits read them.
#[derive(FromRow)]
struct CodeCalls {
    /// The last code call.
    sent_at: Option<DateTime<Utc>>,
    /// Seconds since then.
    sent_ago: Option<i64>,
    /// The day's window: its start, its age in seconds, its code calls.
    window_start: Option<DateTime<Utc>>,
    window_age: Option<i64>,
    window_count: i64,
}

/// When code calls are refused now: the seconds to wait.
fn code_wait(calls: &CodeCalls) -> Option<u64> {
    let wait = |secs: i64| u64::try_from(secs).unwrap_or(0).max(1);
    if let Some(ago) = calls.sent_ago.filter(|ago| *ago < CODE_GAP_SECS) {
        return Some(wait(CODE_GAP_SECS - ago));
    }
    match calls.window_age {
        Some(age) if age < DAY_SECS && calls.window_count >= CODES_PER_DAY => {
            Some(wait(DAY_SECS - age))
        }
        _ => None,
    }
}

/// Verify my cell
///
/// Our server calls `cellNumber` (US or Canada) from a company number and
/// reads out a 6-digit code, valid 10 minutes; send it to
/// `POST /v1/phone/cell/confirm`. The verified cell stays as it is until the
/// new one is confirmed. At most one code call a minute and five a day
/// (429, also when the code was confirmed or voided since), and each takes
/// one of the user's 6 calls a minute (429 while those are used up); a
/// call Telnyx did not place does not count. 400 `invalid_phone`; 403
/// `calling_disabled` while the company's calling is off or it has no
/// number to call from. Takes the device token or the session cookie (the
/// CRM's phone dock verifies the cell for Call via my cell).
#[utoipa::path(
    post,
    path = "/v1/phone/cell/verify",
    operation_id = "verify_phone_cell",
    tag = "phone",
    request_body = CellVerify,
    responses(
        (status = 202, description = "Calling the cell now"),
        (status = 400, description = "`invalid_phone`", body = ErrorBody),
        (status = 401, description = "No valid device token or session", body = ErrorBody),
        (status = 403, description = "`calling_disabled`, or a cookie of a user who is not an employee or admin", body = ErrorBody),
        (status = 429, description = "`rate_limited`: a code call went out less than a minute ago, five today, or 6 calls this minute", body = RateLimited),
        (status = 502, description = "Telnyx did not place the call; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []), ("session_cookie" = []))
)]
pub async fn verify_cell(
    State(state): State<AppState>,
    caller: PhoneCaller,
    JsonBody(input): JsonBody<CellVerify>,
) -> ApiResult<Response> {
    let user = &caller.user;
    let pool = &state.pool;
    let cell = to_e164(&input.cell_number)
        .filter(|cell| is_nanp(cell))
        .ok_or_else(|| bad_request("invalid_phone"))?;
    let config = calling_setup(&state, user.company_id).await?;
    let Some(app_id) = config.texml_app_id.as_deref() else {
        return Err(denied("calling_disabled"));
    };
    let caller_ids = lines::caller_ids(pool, user.company_id, user.id).await?;
    let stored = settings::load(pool, user.id, user.company_id).await?;
    let from = match default_caller_id(&caller_ids, stored.default_caller_number_id) {
        Some(caller) => caller.e164.clone(),
        None => lines::company_number(pool, user.company_id)
            .await?
            .map(|number| number.e164)
            .ok_or_else(|| denied("calling_disabled"))?,
    };
    let code = new_code().map_err(|err| ApiError::Internal(err.to_string()))?;
    let mut tx = pool.begin().await?;
    settings::ensure_row(&mut tx, user.id, user.company_id).await?;
    let calls: CodeCalls = sqlx::query_as(
        "SELECT cell_code_sent_at AS sent_at,
                CAST(TIMESTAMPDIFF(SECOND, cell_code_sent_at, UTC_TIMESTAMP()) AS SIGNED) AS sent_ago,
                cell_code_window_start AS window_start,
                CAST(TIMESTAMPDIFF(SECOND, cell_code_window_start, UTC_TIMESTAMP()) AS SIGNED)
                  AS window_age,
                CAST(cell_code_window_count AS SIGNED) AS window_count
           FROM phone_user_settings WHERE user_id = ? AND company_id = ? FOR UPDATE",
    )
    .bind(user.id)
    .bind(user.company_id)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(wait) = code_wait(&calls) {
        tx.rollback().await?;
        return Ok(rate_limited(wait));
    }
    if let Err(wait) = limits::take_call_slot(&mut tx, user.company_id, user.id, None).await? {
        tx.rollback().await?;
        return Ok(rate_limited(wait.secs()));
    }
    // A new day's window starts with this call.
    let window_open = calls.window_age.is_some_and(|age| age < DAY_SECS);
    sqlx::query(
        "UPDATE phone_user_settings
            SET cell_pending_number = ?, cell_code = ?,
                cell_code_expires_at = UTC_TIMESTAMP() + INTERVAL 10 MINUTE, cell_code_attempts = 0,
                cell_code_sent_at = UTC_TIMESTAMP(),
                cell_code_window_count = IF(?, cell_code_window_count + 1, 1),
                cell_code_window_start = IF(?, cell_code_window_start, UTC_TIMESTAMP())
          WHERE user_id = ? AND company_id = ?",
    )
    .bind(&cell)
    .bind(&code)
    .bind(window_open)
    .bind(window_open)
    .bind(user.id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let url = format!(
        "{}/cell-code?user={}",
        config.webhook_base(&state.phone),
        user.id
    );
    if let Err(err) = state
        .telnyx
        .texml_call(app_id, &cell, &from, &url, None)
        .await
    {
        // The code never went out: it is dropped and the call not counted,
        // so the user may try again at once.
        sqlx::query(
            "UPDATE phone_user_settings
                SET cell_pending_number = NULL, cell_code = NULL, cell_code_expires_at = NULL,
                    cell_code_sent_at = ?, cell_code_window_start = ?, cell_code_window_count = ?
              WHERE user_id = ? AND company_id = ? AND cell_code = ?",
        )
        .bind(calls.sent_at)
        .bind(if window_open {
            calls.window_start
        } else {
            None
        })
        .bind(if window_open { calls.window_count } else { 0 })
        .bind(user.id)
        .bind(user.company_id)
        .bind(&code)
        .execute(pool)
        .await?;
        return Err(telnyx_failed(&err, "cell code call"));
    }
    Ok(StatusCode::ACCEPTED.into_response())
}

/// Confirm my cell
///
/// Checks the code our call read out; when right, the pending number
/// becomes the verified cell (`cellNumber` in the settings returned). 400
/// `wrong_code` for a wrong, expired or used-up code (5 tries, then ask for
/// a new call). Takes the device token or the session cookie.
#[utoipa::path(
    post,
    path = "/v1/phone/cell/confirm",
    operation_id = "confirm_phone_cell",
    tag = "phone",
    request_body = CellConfirm,
    responses(
        (status = 200, description = "Verified; my settings now", body = PhoneSettings),
        (status = 400, description = "`wrong_code`", body = ErrorBody),
        (status = 401, description = "No valid device token or session", body = ErrorBody),
        (status = 403, description = "A cookie of a user who is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []), ("session_cookie" = []))
)]
pub async fn confirm_cell(
    State(state): State<AppState>,
    caller: PhoneCaller,
    JsonBody(input): JsonBody<CellConfirm>,
) -> ApiResult<Json<PhoneSettings>> {
    let user = &caller.user;
    let pool = &state.pool;
    let typed = input.code.trim();
    let mut tx = pool.begin().await?;
    let row: Option<(Option<String>, Option<String>, bool, i64)> = sqlx::query_as(
        "SELECT cell_pending_number, cell_code,
                COALESCE(cell_code_expires_at > UTC_TIMESTAMP(), 0) AS live,
                CAST(cell_code_attempts AS SIGNED)
           FROM phone_user_settings WHERE user_id = ? AND company_id = ? FOR UPDATE",
    )
    .bind(user.id)
    .bind(user.company_id)
    .fetch_optional(&mut *tx)
    .await?;
    let clear = "UPDATE phone_user_settings
                    SET cell_pending_number = NULL, cell_code = NULL, cell_code_expires_at = NULL,
                        cell_code_attempts = 0
                  WHERE user_id = ? AND company_id = ?";
    let Some((Some(pending), Some(expected), true, attempts)) =
        row.filter(|row| row.3 < CODE_TRIES)
    else {
        sqlx::query(clear)
            .bind(user.id)
            .bind(user.company_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Err(bad_request("wrong_code"));
    };
    if !same_code(typed, &expected) {
        if attempts + 1 >= CODE_TRIES {
            sqlx::query(clear)
                .bind(user.id)
                .bind(user.company_id)
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query(
                "UPDATE phone_user_settings SET cell_code_attempts = cell_code_attempts + 1
                  WHERE user_id = ? AND company_id = ?",
            )
            .bind(user.id)
            .bind(user.company_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        return Err(bad_request("wrong_code"));
    }
    sqlx::query(
        "UPDATE phone_user_settings
            SET cell_number = ?, cell_verified_at = UTC_TIMESTAMP(), cell_pending_number = NULL,
                cell_code = NULL, cell_code_expires_at = NULL, cell_code_attempts = 0
          WHERE user_id = ? AND company_id = ?",
    )
    .bind(&pending)
    .bind(user.id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let caller_ids = lines::caller_ids(pool, user.company_id, user.id).await?;
    let stored = settings::load(pool, user.id, user.company_id).await?;
    Ok(Json(stored.wire(&caller_ids)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_calls_wait_a_minute_and_five_a_day() {
        let calls = |sent_ago: Option<i64>, window_age: Option<i64>, window_count: i64| CodeCalls {
            sent_at: None,
            sent_ago,
            window_start: None,
            window_age,
            window_count,
        };
        assert_eq!(code_wait(&calls(None, None, 0)), None);
        assert_eq!(code_wait(&calls(Some(20), Some(20), 1)), Some(40));
        assert_eq!(code_wait(&calls(Some(60), Some(60), 1)), None);
        // The fifth call of the window was an hour ago: wait for the window.
        assert_eq!(code_wait(&calls(Some(3600), Some(80_000), 5)), Some(6400));
        assert_eq!(code_wait(&calls(Some(3600), Some(86_400), 5)), None);
        assert_eq!(code_wait(&calls(Some(3600), Some(80_000), 4)), None);
    }

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..50 {
            let code = new_code().unwrap();
            assert_eq!(code.len(), 6);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
        }
        assert!(same_code("012345", "012345"));
        assert!(!same_code("012345", "012346"));
        assert!(!same_code("012345", "12345"));
    }
}
