//! 911 alerts to the company's admins, and "Report a problem".

use super::{RateLimited, bad_request, rate_limited};
use crate::auth::device::DeviceUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::SUPPORT_EMAIL;
use crate::phone::alerts::{AlertSource, alert_e911};
use crate::phone::numbers::n11_code;
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use lambda_http::tracing;
use regex::{Captures, Regex};
use serde::Deserialize;
use sqlx::MySqlPool;
use std::fmt::Write as _;
use std::sync::LazyLock;

const NO_REPLY: &str = "noreply@granite-manager.com";
/// What a problem report keeps.
const SUMMARY_CHARS: usize = 2000;
const LOG_LINES: usize = 500;
const LOG_LINE_CHARS: usize = 500;
const APP_VERSION_CHARS: usize = 32;
const DEVICE_CHARS: usize = 100;
/// Problem reports a phone may send an hour (a window from its first).
const REPORTS_PER_HOUR: i64 = 3;

/// Body of `POST /v1/phone/e911`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct E911Input {
    /// The emergency or service code dialed: 911, 112, 933, 988, 211 ... 811.
    pub number: String,
}

/// Alert the admins: 911 dialed
///
/// The app opened the phone's own dialer for an emergency or N11 code (the
/// call never goes through Telnyx); this tells the company's admins at once:
/// an in-app notification for each, and an `e911_alert` push to their
/// phones signed in to the company. The caller is not alerted. Alerts are
/// deduplicated for 10 minutes per phone: a repeat of the same code, or
/// any code after an emergency code (911, 112, 933, 988), alerts nobody
/// again (still 204); an emergency code after another N11 code does. 400
/// `invalid_phone` for anything but an N11 or emergency code. Device token
/// only.
#[crate::openapi::path(
    post,
    path = "/v1/phone/e911",
    operation_id = "report_phone_e911",
    tag = "phone",
    request_body = E911Input,
    responses(
        (status = 204, description = "Admins alerted (or alerted in the last 10 minutes already)"),
        (status = 400, description = "`invalid_phone`: not an emergency or N11 code", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn report_e911(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<E911Input>,
) -> ApiResult<StatusCode> {
    let code = n11_code(&input.number).ok_or_else(|| bad_request("invalid_phone"))?;
    let user = &device.user;
    let who = user
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&user.email);
    let source = AlertSource::Session(device.session_id);
    if !alert_e911(&state, user.company_id, source, Some(user.id), who, code).await? {
        tracing::info!(
            session_id = device.session_id,
            "911 alert sent in the last 10 minutes already"
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Takes one of the phone's problem reports this hour: `Err` = seconds to
/// wait.
async fn take_report_slot(pool: &MySqlPool, session_id: i64) -> sqlx::Result<Result<(), u64>> {
    // MySQL assigns left to right: the count reads the window's old start.
    let taken = sqlx::query(
        "UPDATE device_sessions
            SET diagnostics_window_count = IF(diagnostics_window_start IS NULL
                    OR diagnostics_window_start <= UTC_TIMESTAMP() - INTERVAL 1 HOUR,
                    1, diagnostics_window_count + 1),
                diagnostics_window_start = IF(diagnostics_window_start IS NULL
                    OR diagnostics_window_start <= UTC_TIMESTAMP() - INTERVAL 1 HOUR,
                    UTC_TIMESTAMP(), diagnostics_window_start)
          WHERE id = ? AND (diagnostics_window_start IS NULL
                OR diagnostics_window_start <= UTC_TIMESTAMP() - INTERVAL 1 HOUR
                OR diagnostics_window_count < ?)",
    )
    .bind(session_id)
    .bind(REPORTS_PER_HOUR)
    .execute(pool)
    .await?
    .rows_affected();
    if taken > 0 {
        return Ok(Ok(()));
    }
    let left: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(),
                                   diagnostics_window_start + INTERVAL 1 HOUR) AS SIGNED)
           FROM device_sessions WHERE id = ?",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(Err(u64::try_from(left.unwrap_or(60)).unwrap_or(1).max(1)))
}

/// Gives a report slot back (the email did not go out).
async fn give_report_slot_back(pool: &MySqlPool, session_id: i64) {
    let given = sqlx::query(
        "UPDATE device_sessions
            SET diagnostics_window_count = GREATEST(diagnostics_window_count, 1) - 1
          WHERE id = ?",
    )
    .bind(session_id)
    .execute(pool)
    .await;
    if let Err(err) = given {
        tracing::warn!(session_id, ?err, "problem report slot not given back");
    }
}

/// Body of `POST /v1/phone/diagnostics`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsReport {
    /// The app's version.
    pub app_version: String,
    /// The phone, e.g. "samsung SM-S921U, Android 16".
    pub device: String,
    /// What went wrong, in the user's words (at most 2,000 characters are
    /// kept).
    pub summary: String,
    /// The app's recent log lines, numbers masked (at most 500 lines of 500
    /// characters are kept).
    #[serde(default)]
    pub log: Vec<String>,
}

/// Phone numbers in text (E.164, `(317) 555-0142`, `317-555-0142`, 10 or
/// 11 digits in a row).
static PHONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\+\d{7,15}|\(\d{3}\)\s?\d{3}[-.\s]\d{4}|\b\d{3}[-.\s]\d{3}[-.\s]\d{4}\b|\b1?\d{10}\b",
    )
    .expect("phone pattern")
});

/// Long token-like strings (device tokens, push tokens, JWTs).
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9_\-]{40,}(?:\.[A-Za-z0-9_\-]+)*").expect("token pattern")
});

/// `text` with phone numbers cut to their last 4 digits (`…0142`) and
/// tokens removed.
pub fn masked(text: &str) -> String {
    let text = TOKEN.replace_all(text, "[token]");
    PHONE
        .replace_all(&text, |found: &Captures<'_>| {
            let digits: Vec<char> = found[0].chars().filter(char::is_ascii_digit).collect();
            let last4: String = digits[digits.len().saturating_sub(4)..].iter().collect();
            format!("…{last4}")
        })
        .into_owned()
}

/// `text` safe inside HTML.
fn html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn clip(text: &str, max: usize) -> String {
    text.trim().chars().take(max).collect()
}

/// Report a problem
///
/// "Report a problem" from the app's settings: emails support the user's
/// summary, the app version, the phone model and the app's recent log,
/// with phone numbers masked (to their last 4 digits) and tokens removed;
/// replies go to the user. Long reports are cut (2,000 characters of
/// summary, 500 log lines). At most 3 reports an hour per phone (429 with
/// `retryAfterSecs`); a report that could not be sent does not count.
/// Device token only.
#[crate::openapi::path(
    post,
    path = "/v1/phone/diagnostics",
    operation_id = "send_phone_diagnostics",
    tag = "phone",
    request_body = DiagnosticsReport,
    responses(
        (status = 204, description = "Sent to support"),
        (status = 400, description = "Malformed body", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 429, description = "`rate_limited`: 3 reports this hour already", body = RateLimited),
        (status = 502, description = "The email could not be sent; try again", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn send_diagnostics(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(report): JsonBody<DiagnosticsReport>,
) -> ApiResult<Response> {
    if let Err(wait) = take_report_slot(&state.pool, device.session_id).await? {
        return Ok(rate_limited(wait));
    }
    let user = &device.user;
    let company: Option<String> = sqlx::query_scalar("SELECT name FROM company WHERE id = ?")
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?;
    let company = company.unwrap_or_else(|| format!("Company {}", user.company_id));
    let name = user.name.clone().unwrap_or_default();
    let app_version = masked(&clip(&report.app_version, APP_VERSION_CHARS));
    let phone = masked(&clip(&report.device, DEVICE_CHARS));
    let summary = masked(&clip(&report.summary, SUMMARY_CHARS));
    let lines: Vec<String> = report
        .log
        .iter()
        .rev()
        .take(LOG_LINES)
        .rev()
        .map(|line| masked(&line.chars().take(LOG_LINE_CHARS).collect::<String>()))
        .collect();
    let mut body = String::new();
    let _ = write!(
        body,
        "<h2>Granite Phone problem report</h2>\
         <p><b>From:</b> {} &lt;{}&gt; (user {}), {} (company {})</p>\
         <p><b>App:</b> {} on {}</p>\
         <p><b>Summary:</b></p><p style=\"white-space:pre-wrap\">{}</p>\
         <p><b>Log</b> ({} lines, oldest first):</p><pre>{}</pre>",
        html(&name),
        html(&user.email),
        user.id,
        html(&company),
        user.company_id,
        html(&app_version),
        html(&phone),
        html(&summary),
        lines.len(),
        html(&lines.join("\n")),
    );
    let email = OutgoingEmail {
        from_name: Some("Granite Phone".into()),
        from_address: NO_REPLY.into(),
        to: vec![SUPPORT_EMAIL.into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: format!("[Granite Phone] Problem report from {company}"),
        body,
        signature: None,
        reply_to: vec![user.email.clone()],
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    if let Err(err) = state.webhooks.send_email(&email).await {
        tracing::warn!(?err, user_id = user.id, "problem report not sent");
        give_report_slot_back(&state.pool, device.session_id).await;
        return Err(ApiError::Upstream(
            "The report could not be sent. Try again.".into(),
        ));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::masked;

    #[test]
    fn numbers_and_tokens_are_masked() {
        assert_eq!(
            masked("call to +13175550142 failed"),
            "call to …0142 failed"
        );
        assert_eq!(
            masked("from (317) 555-0142 and 317-555-0199, 3175550100 or 13175550101"),
            "from …0142 and …0199, …0100 or …0101"
        );
        // Times, dates, ids and short numbers stay.
        assert_eq!(
            masked("2026-10-18T10:00:00Z call 42 took 12:30, code 911"),
            "2026-10-18T10:00:00Z call 42 took 12:30, code 911"
        );
        assert_eq!(
            masked(&format!("token {} sent", "a1B2_c3-".repeat(6))),
            "token [token] sent"
        );
    }
}
