//! Recents and calls: the list, one call, seen and hidden, talk-time stats,
//! recordings, notes, Call via my cell, test calls and declines.

use super::{RateLimited, bad_request, calling_setup, denied, rate_limited, telnyx_failed};
use crate::auth::device::{DeviceUser, PhoneCaller};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::phone::calls::{self, CallFilter, CallRow, Hearer, NOT_TEST_CALL, RecentsQuery};
use crate::phone::lines::{self, CallerId, default_caller_id};
use crate::phone::numbers::{is_nanp, last10, to_e164};
use crate::phone::{clock, company, contacts, devices, limits, settings};
use crate::routes::customers::service::audit_name;
use crate::routes::deals::service::{Notification, notify_assignees};
use crate::serde_helpers::js_date_required;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use utoipa::{IntoParams, ToSchema};

/// Calls per Recents page when the app asks for no number.
const DEFAULT_PAGE: i64 = 50;
/// Most calls per Recents page.
const MAX_PAGE: i64 = 100;
/// Most ids one `POST /v1/phone/calls/seen` takes.
const MAX_SEEN_IDS: usize = 500;
/// Longest call note.
const MAX_NOTE_CHARS: usize = 5000;

/// Query of `GET /v1/phone/calls`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CallsQuery {
    /// Paging: only calls with ids below this (the previous page's
    /// `nextBefore`).
    pub before: Option<i64>,
    /// Calls per page, 1 to 100 (default 50).
    pub limit: Option<i64>,
    /// `all` (default), `missed`, `incoming`, `outgoing` or `recorded`.
    #[param(inline)]
    pub filter: Option<CallFilter>,
    /// Only calls with this number (E.164 or 10 digits; matched by the last
    /// 10 digits).
    pub number: Option<String>,
}

/// A page of Recents.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallList {
    /// Calls, newest first.
    pub calls: Vec<CallRow>,
    /// Pass as `?before=` for the next page; null on the last page.
    pub next_before: Option<i64>,
}

/// List my Recents
///
/// The caller's calls, newest first: calls they placed or answered, every
/// call of a direct line assigned to them, and unanswered incoming calls of
/// shared and main lines that ring them; calls they hid are left out.
/// `filter`: `missed` = incoming calls nobody answered (voicemails and AI
/// messages included), `incoming`, `outgoing`, `recorded`; `number` = one
/// number's calls. Pages by id (`before`, `limit` 1 to 100, default 50).
/// Device token only.
#[utoipa::path(
    get,
    path = "/v1/phone/calls",
    operation_id = "list_phone_calls",
    tag = "phone",
    params(CallsQuery),
    responses(
        (status = 200, description = "A page of Recents", body = CallList),
        (status = 400, description = "Bad `limit`, `filter` or `number` (`invalid_phone`)", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn list_calls(
    State(state): State<AppState>,
    device: DeviceUser,
    MultiQuery(query): MultiQuery<CallsQuery>,
) -> ApiResult<Json<CallList>> {
    let user = &device.user;
    let limit = query.limit.unwrap_or(DEFAULT_PAGE);
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err(ApiError::BadRequest("limit must be 1 to 100".into()));
    }
    let number = match query.number.as_deref() {
        Some(number) => Some(last10(number).ok_or_else(|| bad_request("invalid_phone"))?),
        None => None,
    };
    let page = usize::try_from(limit).unwrap_or(50);
    let mut calls = calls::recents(
        &state.pool,
        user.company_id,
        Hearer::of(user),
        RecentsQuery {
            filter: query.filter.unwrap_or(CallFilter::All),
            number_last10: number.as_deref(),
            before: query.before,
            limit: page + 1,
        },
    )
    .await?;
    let next_before = if calls.len() > page {
        calls.truncate(page);
        calls.last().map(|call| call.id)
    } else {
        None
    };
    Ok(Json(CallList { calls, next_before }))
}

/// Get a call
///
/// One call of the company as the caller sees it (`seen` is theirs), in
/// their Recents or not; its recording and AI note only when the caller may
/// hear it (`hasRecording`). The native calling module reads it (device
/// header) to word missed-call notifications. 404 for other companies'
/// calls. Device token only.
#[utoipa::path(
    get,
    path = "/v1/phone/calls/{id}",
    operation_id = "get_phone_call",
    tag = "phone",
    params(("id" = i64, Path, description = "Our call id")),
    responses(
        (status = 200, description = "The call", body = CallRow),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 404, description = "No such call in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn get_call(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<CallRow>> {
    calls::call(
        &state.pool,
        device.user.company_id,
        Hearer::of(&device.user),
        id,
    )
    .await?
    .map(Json)
    .ok_or(ApiError::NotFound)
}

/// Whether a call belongs to the company.
async fn company_call(pool: &MySqlPool, company_id: i32, id: i64) -> sqlx::Result<bool> {
    let found: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM phone_calls WHERE id = ? AND company_id = ?")
            .bind(id)
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    Ok(found.is_some())
}

/// Hide a call from my Recents
///
/// Hides the call from the caller's Recents only (others still see it; it
/// stays in contact cards and stats). 404 for other companies' calls.
/// Device token only.
#[utoipa::path(
    delete,
    path = "/v1/phone/calls/{id}",
    operation_id = "hide_phone_call",
    tag = "phone",
    params(("id" = i64, Path, description = "Our call id")),
    responses(
        (status = 204, description = "Hidden"),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 404, description = "No such call in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn hide_call(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    if !company_call(&state.pool, device.user.company_id, id).await? {
        return Err(ApiError::NotFound);
    }
    sqlx::query(
        "INSERT INTO phone_call_views (call_id, user_id, hidden_at) VALUES (?, ?, UTC_TIMESTAMP())
         ON DUPLICATE KEY UPDATE hidden_at = COALESCE(hidden_at, UTC_TIMESTAMP())",
    )
    .bind(id)
    .bind(device.user.id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body of `POST /v1/phone/calls/seen`: `ids` or `upTo`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeenInput {
    /// These calls (up to 500; other companies' ids are ignored).
    #[serde(default)]
    pub ids: Option<Vec<i64>>,
    /// Or every call in my Recents that started at or before this
    /// (RFC 3339).
    #[serde(default)]
    pub up_to: Option<DateTime<Utc>>,
}

/// Mark calls seen
///
/// Marks calls seen for the caller (clears them from the unread counts and
/// shows them as seen): the `ids` sent, or every call in their Recents that
/// started at or before `upTo`. Send one of the two. Device token only.
#[utoipa::path(
    post,
    path = "/v1/phone/calls/seen",
    operation_id = "mark_phone_calls_seen",
    tag = "phone",
    request_body = SeenInput,
    responses(
        (status = 204, description = "Marked"),
        (status = 400, description = "Neither or both of `ids` and `upTo`, or over 500 ids", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn mark_calls_seen(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<SeenInput>,
) -> ApiResult<StatusCode> {
    let user = &device.user;
    let mut insert = sqlx::QueryBuilder::new(
        "INSERT INTO phone_call_views (call_id, user_id, seen_at)
         SELECT pc.id, ",
    );
    insert
        .push_bind(user.id)
        .push(", UTC_TIMESTAMP() FROM phone_calls pc");
    match (input.ids, input.up_to) {
        (Some(ids), None) => {
            if ids.len() > MAX_SEEN_IDS {
                return Err(ApiError::BadRequest("At most 500 ids".into()));
            }
            if ids.is_empty() {
                return Ok(StatusCode::NO_CONTENT);
            }
            insert
                .push(" WHERE pc.company_id = ")
                .push_bind(user.company_id);
            crate::sql::push_in(&mut insert, "pc.id", &ids);
        }
        (None, Some(up_to)) => {
            insert
                .push(" LEFT JOIN phone_call_views v ON v.call_id = pc.id AND v.user_id = ")
                .push_bind(user.id)
                .push(" WHERE v.seen_at IS NULL AND pc.started_at <= ")
                .push_bind(up_to)
                .push(" AND pc.company_id = ")
                .push_bind(user.company_id);
            calls::push_in_recents(&mut insert, user.id);
        }
        _ => {
            return Err(ApiError::BadRequest("Send ids or upTo".into()));
        }
    }
    insert.push(
        " ON DUPLICATE KEY UPDATE
            phone_call_views.seen_at = COALESCE(phone_call_views.seen_at, UTC_TIMESTAMP())",
    );
    insert.build().execute(&state.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/phone/calls/stats`: my answered calls.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallStats {
    /// Talk time today, in seconds.
    pub today_secs: i64,
    /// Talk time this week (from Monday), in seconds.
    pub week_secs: i64,
    /// Talk time this month, in seconds.
    pub month_secs: i64,
    /// Answered calls today.
    pub today_count: i64,
    /// Answered calls this week.
    pub week_count: i64,
    /// Answered calls this month.
    pub month_count: i64,
}

/// Get my call totals
///
/// The caller's answered calls (placed or taken) and their talk time today,
/// this week (from Monday) and this month, in the company's time zone (its
/// business hours' zone, else America/Indiana/Indianapolis). Device token
/// only.
#[utoipa::path(
    get,
    path = "/v1/phone/calls/stats",
    operation_id = "get_phone_call_stats",
    tag = "phone",
    responses(
        (status = 200, description = "My totals", body = CallStats),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn call_stats(
    State(state): State<AppState>,
    device: DeviceUser,
) -> ApiResult<Json<CallStats>> {
    let user = &device.user;
    let config = company::load(&state.pool, user.company_id).await?;
    let tz = company::time_zone_of(config.as_ref());
    let now = Utc::now();
    let today = clock::day_start(tz, now);
    let week = clock::week_start(tz, now);
    let month = clock::month_start(tz, now);
    let (today_secs, week_secs, month_secs, today_count, week_count, month_count): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(COALESCE(SUM(CASE WHEN pc.started_at >= ? THEN COALESCE(pc.duration_secs, 0) END), 0) AS SIGNED),
                CAST(COALESCE(SUM(CASE WHEN pc.started_at >= ? THEN COALESCE(pc.duration_secs, 0) END), 0) AS SIGNED),
                CAST(COALESCE(SUM(CASE WHEN pc.started_at >= ? THEN COALESCE(pc.duration_secs, 0) END), 0) AS SIGNED),
                CAST(COALESCE(SUM(pc.started_at >= ?), 0) AS SIGNED),
                CAST(COALESCE(SUM(pc.started_at >= ?), 0) AS SIGNED),
                CAST(COALESCE(SUM(pc.started_at >= ?), 0) AS SIGNED)
           FROM phone_calls pc
          WHERE pc.company_id = ? AND pc.user_id = ? AND pc.answered_at IS NOT NULL
            AND {NOT_TEST_CALL} AND pc.started_at >= ?"
    )))
    .bind(today)
    .bind(week)
    .bind(month)
    .bind(today)
    .bind(week)
    .bind(month)
    .bind(user.company_id)
    .bind(user.id)
    .bind(week.min(month))
    .fetch_one(&state.pool)
    .await?;
    Ok(Json(CallStats {
        today_secs,
        week_secs,
        month_secs,
        today_count,
        week_count,
        month_count,
    }))
}

/// `GET /v1/phone/calls/{id}/recording`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordingLink {
    /// A link to the recording (works 5 minutes).
    pub url: String,
    /// When the link stops working (RFC 3339).
    #[serde(serialize_with = "js_date_required")]
    pub expires_at: DateTime<Utc>,
    /// The audio's type, e.g. `audio/mpeg`.
    pub content_type: String,
}

/// Get a call's recording
///
/// A link to the call's recording that works 5 minutes. Only for those who
/// may hear the call (403 for others): its user, the users of the direct
/// line it came in on or went out from, for an incoming call nobody
/// answered the users its line rings, the owner of its deal, company admins
/// and sales managers. 404 when the call has no recording (or is another
/// company's). Device token only.
#[utoipa::path(
    get,
    path = "/v1/phone/calls/{id}/recording",
    operation_id = "get_phone_call_recording",
    tag = "phone",
    params(("id" = i64, Path, description = "Our call id")),
    responses(
        (status = 200, description = "The link", body = RecordingLink),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "The caller may not hear this call", body = ErrorBody),
        (status = 404, description = "No such call, or no recording", body = ErrorBody),
        (status = 503, description = "Recordings are not set up on the server", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn call_recording(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<RecordingLink>> {
    let user = &device.user;
    let mut query = sqlx::QueryBuilder::new(
        "SELECT CASE WHEN pc.recording_status = 'copied' THEN pc.recording_key END, ",
    );
    calls::push_may_hear(&mut query, Hearer::of(user));
    query
        .push(" FROM phone_calls pc WHERE pc.id = ")
        .push_bind(id)
        .push(" AND pc.company_id = ")
        .push_bind(user.company_id);
    let row: Option<(Option<String>, bool)> =
        query.build_query_as().fetch_optional(&state.pool).await?;
    let Some((Some(key), may_hear)) = row else {
        return Err(ApiError::NotFound);
    };
    if !may_hear {
        return Err(ApiError::Forbidden);
    }
    let link = crate::phone::recordings::link(&state, &key)
        .await?
        .ok_or_else(|| ApiError::Unavailable("Recordings are not set up.".into()))?;
    Ok(Json(RecordingLink {
        url: link.url,
        expires_at: link.expires_at,
        content_type: link.content_type.into(),
    }))
}

/// Body of `POST /v1/phone/calls/{id}/note`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CallNoteInput {
    /// The note, 1 to 5,000 characters.
    pub text: String,
}

/// `POST /v1/phone/calls/{id}/note`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallNoteCreated {
    /// The new deal note.
    pub note_id: i64,
}

/// Add a note about a call
///
/// Adds a deal note (by the caller) on the call's deal, or else on the
/// oldest open deal of the call's customer, and tells the deal's people
/// like a note written in the CRM. 400 `no_deal` when the call has neither;
/// 404 for other companies' calls. Device token only.
#[utoipa::path(
    post,
    path = "/v1/phone/calls/{id}/note",
    operation_id = "add_phone_call_note",
    tag = "phone",
    params(("id" = i64, Path, description = "Our call id")),
    request_body = CallNoteInput,
    responses(
        (status = 201, description = "Added", body = CallNoteCreated),
        (status = 400, description = "Empty or too long, or `no_deal`", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 404, description = "No such call in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn add_call_note(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<CallNoteInput>,
) -> ApiResult<(StatusCode, Json<CallNoteCreated>)> {
    let user = &device.user;
    let text = input.text.trim();
    if text.is_empty() {
        return Err(ApiError::BadRequest("Note content is required".into()));
    }
    if text.chars().count() > MAX_NOTE_CHARS {
        return Err(ApiError::BadRequest(
            "Note must be 5000 characters or less".into(),
        ));
    }
    let deals: Option<(Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT (SELECT CAST(d.id AS SIGNED) FROM deals d JOIN customers dc ON dc.id = d.customer_id
                  WHERE d.id = pc.deal_id AND d.deleted_at IS NULL AND dc.company_id = pc.company_id),
                (SELECT CAST(MIN(d.id) AS SIGNED) FROM deals d JOIN customers c ON c.id = d.customer_id
                  WHERE d.customer_id = pc.customer_id AND c.company_id = pc.company_id
                    AND c.deleted_at IS NULL AND d.deleted_at IS NULL AND d.is_won IS NULL)
           FROM phone_calls pc WHERE pc.id = ? AND pc.company_id = ?",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    let (call_deal, open_deal) = deals.ok_or(ApiError::NotFound)?;
    let deal_id = call_deal
        .or(open_deal)
        .ok_or_else(|| bad_request("no_deal"))?;
    let note = sqlx::query(
        "INSERT INTO deal_notes (deal_id, company_id, content, created_by) VALUES (?, ?, ?, ?)",
    )
    .bind(deal_id)
    .bind(user.company_id)
    .bind(text)
    .bind(&user.name)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    if let Some(actor) = audit_name(user.name.as_deref()) {
        notify_assignees(
            &state,
            deal_id,
            user.id,
            &actor,
            text,
            Notification::NoteAdded,
        )
        .await;
    }
    Ok((
        StatusCode::CREATED,
        Json(CallNoteCreated {
            note_id: i64::try_from(note).unwrap_or(i64::MAX),
        }),
    ))
}

/// Body of `POST /v1/phone/calls/via-cell`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ViaCellInput {
    /// The number to call, US or Canada (E.164 or 10 digits).
    pub to: String,
    /// The caller ID the customer sees (one of my caller IDs); null = my
    /// default.
    #[serde(default)]
    pub caller_number_id: Option<i32>,
    /// The customer being called, when known; ignored unless a customer of
    /// my company (the number's customer is used then).
    #[serde(default)]
    pub customer_id: Option<i32>,
}

/// A call our server started.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallStarted {
    /// Our call id (`GET /v1/phone/calls/{id}`).
    pub call_id: i64,
}

/// The customer of an outgoing call: the one sent when it is a live
/// customer of the company, else the newest customer with the number.
async fn customer_of_call(
    pool: &MySqlPool,
    company_id: i32,
    sent: Option<i32>,
    to_last10: &str,
) -> sqlx::Result<Option<i32>> {
    if let Some(id) = sent {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM customers
              WHERE id = ? AND company_id = ? AND deleted_at IS NULL AND id <> 990000021",
        )
        .bind(id)
        .bind(company_id)
        .fetch_optional(pool)
        .await?;
        if found.is_some() {
            return Ok(found);
        }
    }
    contacts::customer_for_number(pool, company_id, to_last10).await
}

/// A customer's oldest open deal.
async fn open_deal(pool: &MySqlPool, customer_id: Option<i32>) -> sqlx::Result<Option<i64>> {
    let Some(customer_id) = customer_id else {
        return Ok(None);
    };
    sqlx::query_scalar(
        "SELECT CAST(MIN(id) AS SIGNED) FROM deals
          WHERE customer_id = ? AND deleted_at IS NULL AND is_won IS NULL",
    )
    .bind(customer_id)
    .fetch_one(pool)
    .await
}

/// A new call-log row for a call our server places.
struct NewCall<'a> {
    company_id: i32,
    user_id: i32,
    direction: &'static str,
    other_number: &'a str,
    customer_id: Option<i32>,
    deal_id: Option<i64>,
    caller_id: &'a CallerId,
    via_cell: bool,
    ai_notes: bool,
}

/// Logs a call our server places, in the transaction that took its call
/// slot.
async fn insert_call(conn: &mut sqlx::MySqlConnection, call: &NewCall<'_>) -> sqlx::Result<i64> {
    let id = sqlx::query(
        "INSERT INTO phone_calls
           (company_id, user_id, direction, status, other_number, other_last10, customer_id,
            deal_id, number_id, our_number, caller_id, via_cell, started_at, ai_notes, approved)
         VALUES (?, ?, ?, 'ringing', ?, ?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP(3), ?, 1)",
    )
    .bind(call.company_id)
    .bind(call.user_id)
    .bind(call.direction)
    .bind(call.other_number)
    .bind(last10(call.other_number).unwrap_or_default())
    .bind(call.customer_id)
    .bind(call.deal_id)
    .bind(call.caller_id.number_id)
    .bind(&call.caller_id.e164)
    .bind(&call.caller_id.e164)
    .bind(call.via_cell)
    .bind(call.ai_notes)
    .execute(&mut *conn)
    .await?
    .last_insert_id();
    Ok(i64::try_from(id).unwrap_or(i64::MAX))
}

/// Takes one of the user's call slots and logs the call (one transaction):
/// its id, or how long to wait.
async fn start_call(
    pool: &MySqlPool,
    call: &NewCall<'_>,
    daily: Option<&limits::Daily>,
) -> sqlx::Result<Result<i64, limits::Wait>> {
    let mut tx = pool.begin().await?;
    if let Err(wait) = limits::take_call_slot(&mut tx, call.company_id, call.user_id, daily).await?
    {
        tx.rollback().await?;
        return Ok(Err(wait));
    }
    let id = insert_call(&mut tx, call).await?;
    tx.commit().await?;
    Ok(Ok(id))
}

/// Records that Telnyx took the call (its `CallSid`, unless a callback
/// saved it first), or that it failed.
async fn placed(pool: &MySqlPool, id: i64, sid: Option<&str>) -> sqlx::Result<()> {
    match sid {
        Some(sid) => {
            sqlx::query(
                "UPDATE phone_calls SET telnyx_call_sid = ? WHERE id = ? AND telnyx_call_sid IS NULL",
            )
            .bind(sid)
            .bind(id)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query(
                "UPDATE phone_calls SET status = 'failed', ended_at = UTC_TIMESTAMP(3)
                  WHERE id = ? AND ended_at IS NULL",
            )
            .bind(id)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Call via my cell
///
/// "Call via my cell" (no data on the phone, or from the CRM): our server
/// rings the caller's verified cell from the caller ID; they press 1 and
/// the customer is dialed, recorded and logged (`viaCell` in Recents). The
/// customer is the one sent when it is the company's, else the number's.
/// 400 `invalid_phone` (callable US and Canada numbers only), `no_cell`
/// (no verified cell), `not_a_caller_id`; 403 `calling_disabled`; 429
/// `rate_limited` (6 calls a minute, and the company's daily limit; counted
/// for all the user's phones and the CRM together). Takes the device token
/// or the session cookie.
#[utoipa::path(
    post,
    path = "/v1/phone/calls/via-cell",
    operation_id = "call_phone_via_cell",
    tag = "phone",
    request_body = ViaCellInput,
    responses(
        (status = 202, description = "Ringing my cell now", body = CallStarted),
        (status = 400, description = "`invalid_phone`, `no_cell` or `not_a_caller_id`", body = ErrorBody),
        (status = 401, description = "No valid device token or session", body = ErrorBody),
        (status = 403, description = "`calling_disabled`, or a cookie of a user who is not an employee or admin", body = ErrorBody),
        (status = 429, description = "`rate_limited`", body = RateLimited),
        (status = 502, description = "Telnyx did not place the call; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []), ("session_cookie" = []))
)]
pub async fn call_via_cell(
    State(state): State<AppState>,
    caller: PhoneCaller,
    JsonBody(input): JsonBody<ViaCellInput>,
) -> ApiResult<Response> {
    let user = &caller.user;
    let pool = &state.pool;
    let config = calling_setup(&state, user.company_id).await?;
    let app_id = config
        .texml_app_id
        .clone()
        .ok_or_else(|| denied("calling_disabled"))?;
    let to = to_e164(&input.to)
        .filter(|to| is_nanp(to))
        .ok_or_else(|| bad_request("invalid_phone"))?;
    let stored = settings::load(pool, user.id, user.company_id).await?;
    let cell = stored
        .cell_number
        .clone()
        .ok_or_else(|| bad_request("no_cell"))?;
    let caller_ids = lines::caller_ids(pool, user.company_id, user.id).await?;
    let caller_id = match input.caller_number_id {
        Some(id) => caller_ids.iter().find(|caller| caller.number_id == id),
        None => default_caller_id(&caller_ids, stored.default_caller_number_id),
    }
    .cloned()
    .ok_or_else(|| bad_request("not_a_caller_id"))?;
    let to_last10 = last10(&to).unwrap_or_default();
    let customer_id =
        customer_of_call(pool, user.company_id, input.customer_id, &to_last10).await?;
    let deal_id = open_deal(pool, customer_id).await?;
    let tz = company::time_zone_of(Some(&config));
    let now = Utc::now();
    let daily = limits::Daily {
        limit: config.daily_call_limit,
        since: clock::day_start(tz, now),
        until: clock::next_day_start(tz, now),
    };
    let new_call = NewCall {
        company_id: user.company_id,
        user_id: user.id,
        direction: "outbound",
        other_number: &to,
        customer_id,
        deal_id,
        caller_id: &caller_id,
        via_cell: true,
        ai_notes: stored.ai_notes,
    };
    let id = match start_call(pool, &new_call, Some(&daily)).await? {
        Ok(id) => id,
        Err(wait) => return Ok(rate_limited(wait.secs())),
    };
    let base = config.webhook_base(&state.phone);
    let result = state
        .telnyx
        .texml_call(
            &app_id,
            &cell,
            &caller_id.e164,
            &format!("{base}/via-cell?call={id}"),
            Some(&format!("{base}/call-status?call={id}")),
        )
        .await;
    match result {
        Ok(sid) => {
            if let Some(sid) = sid {
                placed(pool, id, Some(&sid)).await?;
            }
            Ok((StatusCode::ACCEPTED, Json(CallStarted { call_id: id })).into_response())
        }
        Err(err) => {
            placed(pool, id, None).await?;
            Err(telnyx_failed(&err, "via-cell call"))
        }
    }
}

/// Test this phone
///
/// The call health check: our server calls this phone (its SIP user) from
/// the caller's default caller ID (else a company number) and plays a short
/// message. Test calls are not in Recents or stats; follow it with
/// `GET /v1/phone/calls/{id}`. 400 when the phone is not registered; 403
/// `calling_disabled`; 429 `rate_limited` (6 calls a minute). Device token
/// only.
#[utoipa::path(
    post,
    path = "/v1/phone/calls/test",
    operation_id = "test_phone_call",
    tag = "phone",
    responses(
        (status = 202, description = "Calling this phone now", body = CallStarted),
        (status = 400, description = "This phone is not registered for calls", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "`calling_disabled`", body = ErrorBody),
        (status = 429, description = "`rate_limited`", body = RateLimited),
        (status = 502, description = "Telnyx did not place the call; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn test_call(State(state): State<AppState>, device: DeviceUser) -> ApiResult<Response> {
    let user = &device.user;
    let pool = &state.pool;
    let config = calling_setup(&state, user.company_id).await?;
    let app_id = config
        .texml_app_id
        .clone()
        .ok_or_else(|| denied("calling_disabled"))?;
    let device_id = device.device_id.ok_or_else(|| {
        ApiError::BadRequest("Register this phone first (POST /v1/phone/devices).".into())
    })?;
    let (_, sip_username) = devices::ensure_credential(&state, Some(&config), device_id)
        .await
        .map_err(|err| match err {
            devices::CredentialError::Telnyx(err) => telnyx_failed(&err, "credential"),
            devices::CredentialError::Database(err) => ApiError::Database(err),
        })?
        .ok_or_else(|| denied("calling_disabled"))?;
    let caller_ids = lines::caller_ids(pool, user.company_id, user.id).await?;
    let stored = settings::load(pool, user.id, user.company_id).await?;
    let from = match default_caller_id(&caller_ids, stored.default_caller_number_id) {
        Some(caller) => caller.clone(),
        None => lines::company_number(pool, user.company_id)
            .await?
            .ok_or_else(|| denied("calling_disabled"))?,
    };
    // Our number calling our own phone: a test call (never in Recents).
    let new_call = NewCall {
        company_id: user.company_id,
        user_id: user.id,
        direction: "inbound",
        other_number: &from.e164,
        customer_id: None,
        deal_id: None,
        caller_id: &from,
        via_cell: false,
        ai_notes: false,
    };
    let id = match start_call(pool, &new_call, None).await? {
        Ok(id) => id,
        Err(wait) => return Ok(rate_limited(wait.secs())),
    };
    let base = config.webhook_base(&state.phone);
    let result = state
        .telnyx
        .texml_call(
            &app_id,
            &format!("sip:{sip_username}@sip.telnyx.com"),
            &from.e164,
            &format!("{base}/test-call?call={id}"),
            Some(&format!("{base}/call-status?call={id}")),
        )
        .await;
    match result {
        Ok(sid) => {
            if let Some(sid) = sid {
                placed(pool, id, Some(&sid)).await?;
            }
            Ok((StatusCode::ACCEPTED, Json(CallStarted { call_id: id })).into_response())
        }
        Err(err) => {
            placed(pool, id, None).await?;
            Err(telnyx_failed(&err, "test call"))
        }
    }
}

/// Body of `POST /v1/phone/calls/decline`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeclineInput {
    /// The call id of Telnyx's incoming-call push.
    pub telnyx_call_id: String,
}

/// Decline an incoming call
///
/// Best effort, after the user declined on this phone: the call's leg to
/// this phone is marked declined, and when the call still rings, nobody
/// answered and no other person's phone rings, the caller goes on to the
/// number's no-answer action (AI Assistant, voicemail or forward), which
/// stops the user's other phones. On a shared line that also rings others,
/// only this phone stops. Unknown ids are ignored. Device token only.
#[utoipa::path(
    post,
    path = "/v1/phone/calls/decline",
    operation_id = "decline_phone_call",
    tag = "phone",
    request_body = DeclineInput,
    responses(
        (status = 204, description = "Done (or nothing to do)"),
        (status = 400, description = "Empty id", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn decline_call(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<DeclineInput>,
) -> ApiResult<StatusCode> {
    let telnyx_call_id = input.telnyx_call_id.trim();
    if telnyx_call_id.is_empty() {
        return Err(ApiError::BadRequest("telnyxCallId is required".into()));
    }
    let Some(device_id) = device.device_id else {
        return Ok(StatusCode::NO_CONTENT);
    };
    let user = &device.user;
    let pool = &state.pool;
    let leg: Option<(i64, i64, Option<String>, bool)> = sqlx::query_as(
        "SELECT CAST(l.id AS SIGNED), CAST(pc.id AS SIGNED), pc.telnyx_call_sid,
                (pc.status = 'ringing' AND pc.answered_at IS NULL
                 AND NOT EXISTS (SELECT 1 FROM phone_call_legs a
                                  WHERE a.call_id = pc.id
                                    AND a.status IN ('answered', 'in-progress', 'in_progress'))
                 AND NOT EXISTS (SELECT 1 FROM phone_call_legs o
                                   JOIN phone_devices od ON od.id = o.device_id
                                  WHERE o.call_id = pc.id AND od.user_id <> ?
                                    AND o.status IN ('queued', 'initiated', 'ringing'))) AS stop
           FROM phone_call_legs l JOIN phone_calls pc ON pc.id = l.call_id
          WHERE l.leg_call_sid = ? AND l.device_id = ? AND pc.company_id = ?",
    )
    .bind(user.id)
    .bind(telnyx_call_id)
    .bind(device_id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    let Some((leg_id, call_id, parent_sid, stop)) = leg else {
        return Ok(StatusCode::NO_CONTENT);
    };
    sqlx::query("UPDATE phone_call_legs SET status = 'declined' WHERE id = ?")
        .bind(leg_id)
        .execute(pool)
        .await?;
    if !stop {
        return Ok(StatusCode::NO_CONTENT);
    }
    let config = company::load(pool, user.company_id).await?;
    let (Some(config), Some(parent_sid)) = (config, parent_sid) else {
        return Ok(StatusCode::NO_CONTENT);
    };
    let url = format!(
        "{}/dial-result?call={call_id}&declined=1",
        config.webhook_base(&state.phone)
    );
    if let Err(err) = state.telnyx.texml_redirect(&parent_sid, &url).await {
        tracing::warn!(call_id, error = %err, "declined call not redirected");
    }
    Ok(StatusCode::NO_CONTENT)
}
