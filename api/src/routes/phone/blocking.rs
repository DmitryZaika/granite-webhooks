//! The company's spam and blocked numbers (admins, in the app):
//! `GET|PUT /v1/phone/call-blocking`. What they do is `phone::blocking`.

use super::{audit, bad_request};
use crate::auth::device::DeviceUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::blocking::{
    self, BlockedAction, CallBlocking, DEFAULT_ANNOUNCEMENT, MAX_ANNOUNCEMENT_CHARS, MAX_ENTRIES,
    SpamAction,
};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::json;
use utoipa::ToSchema;

/// Body of `PUT /v1/phone/call-blocking`: the whole setting (the screen's
/// SAVE).
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallBlockingInput {
    /// What happens to likely spam: `ring` (allow to ring), `block` (the
    /// action for blocked calls) or `voicemail`.
    pub spam_action: SpamAction,
    /// What blocked callers get: `announce` (the announcement, then hang
    /// up), `hangup` or `voicemail`.
    pub blocked_action: BlockedAction,
    /// What blocked callers hear with `announce`, at most 500 characters;
    /// null, empty or the default text = the default announcement.
    #[serde(default)]
    pub announcement: Option<String>,
    /// Block calls that hide their number.
    #[serde(default)]
    pub block_no_caller_id: bool,
    /// Blocked numbers (any US or Canada format, or E.164) and 3-digit US
    /// or Canada area codes; at most 1000; duplicates are dropped.
    #[serde(default)]
    pub blocked: Vec<String>,
    /// Trusted numbers, never blocked (numbers only); at most 1000.
    #[serde(default)]
    pub trusted: Vec<String>,
}

/// The caller may change the company's blocking: a company admin (super
/// admins and superusers too); else 403 `admin_only`.
fn admin(device: &DeviceUser) -> ApiResult<()> {
    if device.user.is_admin || device.user.is_superuser {
        Ok(())
    } else {
        Err(ApiError::Denied("admin_only".into()))
    }
}

/// Entries as saved: each normalized, sorted, without duplicates; 400
/// `invalid_entry` naming the first one that is not a number (or area
/// code).
fn entries(list: &[String], area_codes: bool) -> ApiResult<Vec<String>> {
    if list.len() > MAX_ENTRIES {
        return Err(ApiError::BadRequest(format!(
            "At most {MAX_ENTRIES} numbers in a list."
        )));
    }
    let mut out = Vec::with_capacity(list.len());
    for entry in list {
        out.push(
            blocking::normalize_entry(entry, area_codes)
                .ok_or_else(|| bad_request("invalid_entry"))?,
        );
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Read the company's spam and blocked numbers
///
/// Spam call handling, the blocked numbers and area codes, the action for
/// blocked calls and its announcement, the trusted numbers, and whether
/// calls with no caller ID are blocked (the defaults until an admin saves:
/// spam rings, blocked callers hear the default announcement, nothing is
/// blocked). Company admins only (403 `admin_only`). Device token only.
#[utoipa::path(
    get,
    path = "/v1/phone/call-blocking",
    operation_id = "get_phone_call_blocking",
    tag = "phone",
    responses(
        (status = 200, description = "The company's call blocking", body = CallBlocking),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "`admin_only`", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn get_call_blocking(
    State(state): State<AppState>,
    device: DeviceUser,
) -> ApiResult<Json<CallBlocking>> {
    admin(&device)?;
    let stored = blocking::load(&state.pool, device.user.company_id).await?;
    Ok(Json(stored.wire()))
}

/// Save the company's spam and blocked numbers
///
/// Replaces the company's whole call blocking with the one sent, for every
/// incoming call from now on: it applies before anything rings, and blocked
/// calls get no missed-call push. A trusted number is never blocked, not
/// even as spam. Returns it as saved (entries normalized to E.164 and
/// sorted). Audited. Company admins only (403 `admin_only`). 400
/// `invalid_entry` for an entry that is not a number (or, for blocked, a
/// 3-digit area code), or what is wrong. Device token only.
#[utoipa::path(
    put,
    path = "/v1/phone/call-blocking",
    operation_id = "save_phone_call_blocking",
    tag = "phone",
    request_body = CallBlockingInput,
    responses(
        (status = 200, description = "The company's call blocking now", body = CallBlocking),
        (status = 400, description = "`invalid_entry`, or what is wrong", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "`admin_only`", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn save_call_blocking(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<CallBlockingInput>,
) -> ApiResult<Json<CallBlocking>> {
    admin(&device)?;
    let user = &device.user;
    let blocked = entries(&input.blocked, true)?;
    let trusted = entries(&input.trusted, false)?;
    let announcement = input
        .announcement
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty() && *text != DEFAULT_ANNOUNCEMENT);
    if announcement.is_some_and(|text| text.chars().count() > MAX_ANNOUNCEMENT_CHARS) {
        return Err(ApiError::BadRequest(
            "The announcement is at most 500 characters.".into(),
        ));
    }
    let pool = &state.pool;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO phone_call_blocking
           (company_id, spam_action, blocked_action, announcement, block_no_caller_id, updated_by)
         VALUES (?, ?, ?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE spam_action = VALUES(spam_action),
           blocked_action = VALUES(blocked_action), announcement = VALUES(announcement),
           block_no_caller_id = VALUES(block_no_caller_id), updated_by = VALUES(updated_by)",
    )
    .bind(user.company_id)
    .bind(input.spam_action.as_str())
    .bind(input.blocked_action.as_str())
    .bind(announcement)
    .bind(input.block_no_caller_id)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM phone_call_block_list WHERE company_id = ?")
        .bind(user.company_id)
        .execute(&mut *tx)
        .await?;
    for (list, patterns) in [("blocked", &blocked), ("trusted", &trusted)] {
        for chunk in patterns.chunks(200) {
            let mut insert = sqlx::QueryBuilder::new(
                "INSERT INTO phone_call_block_list (company_id, list, pattern, created_by) ",
            );
            insert.push_values(chunk, |mut row, pattern| {
                row.push_bind(user.company_id)
                    .push_bind(list)
                    .push_bind(pattern)
                    .push_bind(user.id);
            });
            insert.build().execute(&mut *tx).await?;
        }
    }
    // Counts only: the audit log keeps no phone numbers.
    audit(
        &mut *tx,
        user.company_id,
        user.id,
        "call_blocking.save",
        "company",
        &json!({
            "spamAction": input.spam_action.as_str(),
            "blockedAction": input.blocked_action.as_str(),
            "blockNoCallerId": input.block_no_caller_id,
            "blocked": blocked.len(),
            "trusted": trusted.len(),
        }),
    )
    .await?;
    tx.commit().await?;
    let stored = blocking::load(pool, user.company_id).await?;
    Ok(Json(stored.wire()))
}
