//! My caller IDs, phone settings, availability and accepted notices.

use super::bad_request;
use crate::auth::device::DeviceUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::availability::{Availability, WorkingHours};
use crate::phone::lines::{self, CallerId, default_caller_id};
use crate::phone::settings::{
    self, E911_NOTICE_VERSION, NoticeKind, PhoneSettings, RECORDING_NOTICE_VERSION,
    RING_TIMEOUT_SECS,
};
use crate::phone::wire_enum;
use crate::serde_helpers::double_option;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Duration, SubsecRound, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// `GET /v1/phone/caller-ids`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallerIds {
    /// The numbers the user may call from: their direct lines first, then
    /// shared lines, then the main line.
    pub caller_ids: Vec<CallerId>,
    /// The one to preselect: the user's chosen default while it is still
    /// theirs, else their first direct line, else the first one; null when
    /// they may call from none.
    pub default_number_id: Option<i32>,
}

/// List my caller IDs
///
/// The company numbers the user may call from (direct lines first, then
/// shared lines, then the main line) and the one to preselect. Device token
/// only.
#[utoipa::path(
    get,
    path = "/v1/phone/caller-ids",
    operation_id = "list_phone_caller_ids",
    tag = "phone",
    responses(
        (status = 200, description = "My caller IDs", body = CallerIds),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn list_caller_ids(
    State(state): State<AppState>,
    device: DeviceUser,
) -> ApiResult<Json<CallerIds>> {
    let user = &device.user;
    let caller_ids = lines::caller_ids(&state.pool, user.company_id, user.id).await?;
    let stored = settings::load(&state.pool, user.id, user.company_id).await?;
    let default_number_id = default_caller_id(&caller_ids, stored.default_caller_number_id)
        .map(|caller| caller.number_id);
    Ok(Json(CallerIds {
        caller_ids,
        default_number_id,
    }))
}

/// Body of `PUT /v1/phone/settings`: only the fields sent change.
// `Option<Option<_>>`: a field left out keeps its value, `null` clears it.
#[allow(clippy::option_option)]
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    /// My default caller ID: one of `GET /v1/phone/caller-ids` (400
    /// `not_a_caller_id` otherwise); null clears it.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<i32>)]
    pub default_caller_number_id: Option<Option<i32>>,
    /// Transcribe my recorded calls and add AI notes to deals.
    #[serde(default)]
    pub ai_notes: Option<bool>,
    /// When I take calls; null clears them (always available).
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<WorkingHours>)]
    pub working_hours: Option<Option<WorkingHours>>,
    /// Ring my cell when the app does not answer; turning it on needs a
    /// verified cell (400 `cell_not_verified`).
    #[serde(default)]
    pub cell_fallback: Option<bool>,
    /// How long calls ring my phones, 15 to 60 seconds.
    #[serde(default)]
    pub ring_timeout_secs: Option<i64>,
}

/// Change my phone settings
///
/// Changes only the fields sent: default caller ID, AI notes, working
/// hours, cell fallback, ring timeout; returns all the settings. The cell
/// number changes only through `POST /v1/phone/cell/verify` and `/confirm`.
/// 400 `not_a_caller_id`, `cell_not_verified`, or a message for bad hours or
/// timeouts. Device token only.
#[utoipa::path(
    put,
    path = "/v1/phone/settings",
    operation_id = "update_phone_settings",
    tag = "phone",
    request_body = SettingsPatch,
    responses(
        (status = 200, description = "The settings now", body = PhoneSettings),
        (status = 400, description = "`not_a_caller_id`, `cell_not_verified`, or what is wrong", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn update_settings(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(patch): JsonBody<SettingsPatch>,
) -> ApiResult<Json<PhoneSettings>> {
    let user = &device.user;
    let pool = &state.pool;
    let caller_ids = lines::caller_ids(pool, user.company_id, user.id).await?;
    let current = settings::load(pool, user.id, user.company_id).await?;
    if let Some(Some(id)) = patch.default_caller_number_id
        && !caller_ids.iter().any(|caller| caller.number_id == id)
    {
        return Err(bad_request("not_a_caller_id"));
    }
    let working_hours = match patch.working_hours {
        Some(Some(hours)) => {
            if let Some(problem) = hours.problem() {
                return Err(ApiError::BadRequest(problem));
            }
            Some(Some(WorkingHours {
                tz: hours.tz.trim().to_string(),
                days: hours.days,
            }))
        }
        other => other,
    };
    if patch.cell_fallback == Some(true) && current.cell_number.is_none() {
        return Err(bad_request("cell_not_verified"));
    }
    if patch
        .ring_timeout_secs
        .is_some_and(|secs| !RING_TIMEOUT_SECS.contains(&secs))
    {
        return Err(ApiError::BadRequest(
            "ringTimeoutSecs must be 15 to 60".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    settings::ensure_row(&mut tx, user.id, user.company_id).await?;
    let mut update =
        sqlx::QueryBuilder::new("UPDATE phone_user_settings SET updated_at = UTC_TIMESTAMP()");
    if let Some(default) = patch.default_caller_number_id {
        update
            .push(", default_caller_number_id = ")
            .push_bind(default);
    }
    if let Some(ai_notes) = patch.ai_notes {
        update.push(", ai_notes = ").push_bind(ai_notes);
    }
    if let Some(hours) = working_hours {
        update
            .push(", working_hours = ")
            .push_bind(hours.as_ref().map(WorkingHours::to_json));
    }
    if let Some(cell_fallback) = patch.cell_fallback {
        update.push(", cell_fallback = ").push_bind(cell_fallback);
    }
    if let Some(secs) = patch.ring_timeout_secs {
        update.push(", ring_timeout_secs = ").push_bind(secs);
    }
    update
        .push(" WHERE user_id = ")
        .push_bind(user.id)
        .push(" AND company_id = ")
        .push_bind(user.company_id);
    update.build().execute(&mut *tx).await?;
    tx.commit().await?;
    let stored = settings::load(pool, user.id, user.company_id).await?;
    Ok(Json(stored.wire(&caller_ids)))
}

wire_enum! {
    /// What the user chooses: `available` or `dnd` (Do not disturb).
    pub enum AvailabilityChoice {
        /// Calls ring my phones (within my working hours).
        Available = "available",
        /// Do not disturb.
        Dnd = "dnd",
    }
}

/// Body of `PUT /v1/phone/availability`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AvailabilityInput {
    /// `available` (also ends Do not disturb) or `dnd`.
    pub status: AvailabilityChoice,
    /// With `dnd`: when it ends (RFC 3339, in the future, within a year);
    /// null or left out = until turned off. Ignored with `available`.
    #[serde(default)]
    pub dnd_until: Option<DateTime<Utc>>,
}

/// Set my availability
///
/// `available` ends Do not disturb; `dnd` turns it on until `dndUntil`, or
/// until turned off when that is null. Returns the availability now, which
/// is `outside_hours` when the user is available but outside their working
/// hours. While not available, calls go to the number's fallback (and still
/// show in Recents). Device token only.
#[utoipa::path(
    put,
    path = "/v1/phone/availability",
    operation_id = "set_phone_availability",
    tag = "phone",
    request_body = AvailabilityInput,
    responses(
        (status = 200, description = "My availability now", body = Availability),
        (status = 400, description = "Unknown status, or `dndUntil` not in the next year", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn set_availability(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<AvailabilityInput>,
) -> ApiResult<Json<Availability>> {
    let user = &device.user;
    let now = Utc::now();
    let (dnd_until, dnd_indefinite) = match (input.status, input.dnd_until) {
        (AvailabilityChoice::Available, _) => (None, false),
        (AvailabilityChoice::Dnd, None) => (None, true),
        (AvailabilityChoice::Dnd, Some(until)) => {
            if until <= now || until > now + Duration::days(366) {
                return Err(ApiError::BadRequest(
                    "dndUntil must be in the next year".into(),
                ));
            }
            (Some(until.trunc_subsecs(0)), false)
        }
    };
    let mut tx = state.pool.begin().await?;
    settings::ensure_row(&mut tx, user.id, user.company_id).await?;
    sqlx::query(
        "UPDATE phone_user_settings SET dnd_until = ?, dnd_indefinite = ?
          WHERE user_id = ? AND company_id = ?",
    )
    .bind(dnd_until)
    .bind(dnd_indefinite)
    .bind(user.id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let stored = settings::load(&state.pool, user.id, user.company_id).await?;
    Ok(Json(stored.availability(now)))
}

/// Body of `POST /v1/phone/notices/accept`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NoticeAcceptance {
    /// `recording` or `e911`.
    pub notice: NoticeKind,
    /// The version shown (from `GET /v1/phone/bootstrap` `notices`).
    pub version: i64,
}

/// Accept a notice
///
/// Records that the user accepted this version of the recording notice or
/// the 911 notice (on this phone); accepting again is fine. 400 for a
/// version that does not exist (below 1 or above the current one). Device
/// token only.
#[utoipa::path(
    post,
    path = "/v1/phone/notices/accept",
    operation_id = "accept_phone_notice",
    tag = "phone",
    request_body = NoticeAcceptance,
    responses(
        (status = 204, description = "Accepted"),
        (status = 400, description = "Unknown notice or version", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn accept_notice(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<NoticeAcceptance>,
) -> ApiResult<StatusCode> {
    let current = match input.notice {
        NoticeKind::Recording => RECORDING_NOTICE_VERSION,
        NoticeKind::E911 => E911_NOTICE_VERSION,
    };
    let version = i32::try_from(input.version)
        .ok()
        .filter(|version| (1..=current).contains(version))
        .ok_or_else(|| ApiError::BadRequest(format!("version must be 1 to {current}")))?;
    sqlx::query(
        "INSERT IGNORE INTO phone_notice_acks (user_id, device_id, notice, version)
         VALUES (?, ?, ?, ?)",
    )
    .bind(device.user.id)
    .bind(device.device_id)
    .bind(input.notice.as_str())
    .bind(version)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
