//! The Record button of a live call (the CRM's phone dock; the app later):
//! `PUT /v1/phone/calls/recording`.
//!
//! A call has one recording, copied to our bucket when the call ends (the
//! usual path: `call.recording.saved` for the app's and the browser's calls,
//! the TeXML `recording` callback for the others). The button starts it
//! (outgoing calls when the company records incoming calls only, after the
//! notice is played to both sides), pauses it and resumes it; a paused part
//! is left out of the recording.

use super::{denied, telnyx_failed};
use crate::auth::device::PhoneCaller;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::company::{self, RecordingMode};
use crate::phone::numbers::is_nanp;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// What both sides hear when a recording starts or resumes (unless the
/// company set a notice recording of its own).
const RECORDING_NOW: &str = "This call is being recorded.";

/// Body of `PUT /v1/phone/calls/recording`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordingInput {
    /// True: record (start, or resume after a pause); false: pause.
    pub recording: bool,
    /// The ids Telnyx's SDK knows the caller's call by (its call control
    /// id, call session id, leg id; at most 4 are read), to pick the call
    /// when the caller has more than one; empty = the newest.
    #[serde(default)]
    pub telnyx_call_ids: Vec<String>,
}

/// `PUT /v1/phone/calls/recording`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LiveRecording {
    /// Our call id.
    pub call_id: i64,
    /// The call is being recorded now.
    pub recording: bool,
}

/// A live call of the caller, as the button needs it.
#[derive(Debug, FromRow)]
struct LiveCall {
    id: i64,
    direction: String,
    via_cell: bool,
    other_number: String,
    our_number: Option<String>,
    telnyx_call_sid: Option<String>,
    telnyx_call_control_id: Option<String>,
    telnyx_call_session_id: Option<String>,
    live_recording: String,
    live_recording_sid: Option<String>,
    /// The leg of the phone that answered (calls that rang our phones).
    answered_leg_sid: Option<String>,
}

impl LiveCall {
    fn named_by(&self, ids: &[String]) -> bool {
        [
            &self.telnyx_call_sid,
            &self.telnyx_call_control_id,
            &self.telnyx_call_session_id,
            &self.answered_leg_sid,
        ]
        .into_iter()
        .flatten()
        .any(|id| ids.iter().any(|given| given == id))
    }
}

/// Record (or pause) my live call
///
/// The Record button of a call in progress: `recording: true` starts the
/// call's recording (an outgoing call when the company records incoming
/// calls only: both sides first hear "This call is being recorded.", or the
/// company's notice) or resumes it after a pause (the notice again on the
/// app's and the browser's calls); `recording: false` pauses it (the paused
/// part is left out). A call has one recording, copied to the private
/// bucket when the call ends like any other. The call is the caller's
/// answered call that has not ended, the one the given Telnyx ids name
/// (else the newest). 403 `recording_off` when the company records no
/// calls, `calling_disabled` while its calling is off; 404 when the caller
/// is on no call; 409 `recording_unavailable` when this call cannot be
/// recorded here (an emergency or test call, a Call via my cell call the
/// company does not record, a TeXML call without the company's Telnyx
/// account id saved, or no recording to resume); 502 when Telnyx failed.
/// Takes the device token or the session cookie.
#[crate::openapi::path(
    put,
    path = "/v1/phone/calls/recording",
    operation_id = "set_live_call_recording",
    tag = "phone",
    request_body = RecordingInput,
    responses(
        (status = 200, description = "Recording (or paused) now", body = LiveRecording),
        (status = 401, description = "No valid device token or session", body = ErrorBody),
        (status = 403, description = "`recording_off`, `calling_disabled`, or a cookie of a user who is not an employee or admin", body = ErrorBody),
        (status = 404, description = "The caller is on no call", body = ErrorBody),
        (status = 409, description = "`recording_unavailable`", body = ErrorBody),
        (status = 502, description = "Telnyx did not take the command; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []), ("session_cookie" = []))
)]
pub async fn set_live_call_recording(
    State(state): State<AppState>,
    caller: PhoneCaller,
    JsonBody(input): JsonBody<RecordingInput>,
) -> ApiResult<Json<LiveRecording>> {
    let user = &caller.user;
    let pool = &state.pool;
    let config = company::load(pool, user.company_id)
        .await?
        .filter(company::CompanyConfig::calling_on)
        .ok_or_else(|| denied("calling_disabled"))?;
    if config.recording_mode == RecordingMode::Off {
        return Err(denied("recording_off"));
    }
    if !state.telnyx.is_enabled() {
        return Err(ApiError::Unavailable(
            "Calling is not set up on the server.".into(),
        ));
    }
    let calls: Vec<LiveCall> = sqlx::query_as(
        "SELECT CAST(pc.id AS SIGNED) AS id, CAST(pc.direction AS CHAR) AS direction,
                pc.via_cell <> 0 AS via_cell, pc.other_number, pc.our_number,
                pc.telnyx_call_sid, pc.telnyx_call_control_id, pc.telnyx_call_session_id,
                CAST(pc.live_recording AS CHAR) AS live_recording, pc.live_recording_sid,
                (SELECT l.leg_call_sid FROM phone_call_legs l
                  WHERE l.call_id = pc.id AND l.device_id = pc.answered_device_id
                  ORDER BY l.id DESC LIMIT 1) AS answered_leg_sid
           FROM phone_calls pc
          WHERE pc.company_id = ? AND pc.user_id = ? AND pc.answered_at IS NOT NULL
            AND pc.ended_at IS NULL AND pc.started_at > UTC_TIMESTAMP() - INTERVAL 6 HOUR
          ORDER BY pc.answered_at DESC, pc.id DESC
          LIMIT 5",
    )
    .bind(user.company_id)
    .bind(user.id)
    .fetch_all(pool)
    .await?;
    let ids: Vec<String> = input
        .telnyx_call_ids
        .iter()
        .map(|id| id.trim().chars().take(128).collect::<String>())
        .filter(|id| !id.is_empty())
        .take(4)
        .collect();
    let index = calls
        .iter()
        .position(|call| call.named_by(&ids))
        .unwrap_or(0);
    let call = calls.into_iter().nth(index).ok_or(ApiError::NotFound)?;
    let unavailable = || ApiError::Conflict("recording_unavailable".into());
    let test = call.our_number.as_deref() == Some(call.other_number.as_str());
    if test || !is_nanp(&call.other_number) {
        return Err(unavailable());
    }
    let inbound = call.direction == "inbound";
    let by_mode = config.recording_mode == RecordingMode::Both
        || (inbound && config.recording_mode == RecordingMode::Inbound);
    let now_recording = match call.live_recording.as_str() {
        "recording" => true,
        "paused" => false,
        _ => by_mode,
    };
    if input.recording == now_recording {
        return Ok(Json(LiveRecording {
            call_id: call.id,
            recording: now_recording,
        }));
    }
    let command = format!("record-{}-{}", call.id, Utc::now().timestamp_millis());
    let mut saved_sid = None;
    match (&call.telnyx_call_control_id, inbound || call.via_cell) {
        // The app's and the browser's calls: Call Control on our leg.
        (Some(leg), false) => {
            let done = if input.recording {
                let notice = match config.recording_notice_url.as_deref() {
                    Some(url) => {
                        state
                            .telnyx
                            .play_audio(leg, url, &format!("{command}-n"))
                            .await
                    }
                    None => {
                        state
                            .telnyx
                            .speak(leg, RECORDING_NOW, &format!("{command}-n"))
                            .await
                    }
                };
                match notice {
                    Err(err) => Err(err),
                    Ok(()) if call.live_recording == "paused" => {
                        state.telnyx.record_pause(leg, false, &command).await
                    }
                    Ok(()) => state.telnyx.record_start(leg, &command).await,
                }
            } else {
                state.telnyx.record_pause(leg, true, &command).await
            };
            done.map_err(|err| telnyx_failed(&err, "recording"))?;
        }
        // Calls our TeXML app answered or placed: their recording runs from
        // the answer (with the notice); it can be paused and resumed only.
        _ => {
            let (Some(account), Some(call_sid)) = (
                config.telnyx_account_id.as_deref(),
                call.telnyx_call_sid.as_deref(),
            ) else {
                return Err(unavailable());
            };
            let sid = if input.recording {
                if call.live_recording != "paused" {
                    return Err(unavailable());
                }
                call.live_recording_sid.clone().ok_or_else(unavailable)?
            } else {
                let mut found = None;
                for sid in [Some(call_sid), call.answered_leg_sid.as_deref()]
                    .into_iter()
                    .flatten()
                {
                    let recordings = state
                        .telnyx
                        .texml_recordings(account, sid)
                        .await
                        .map_err(|err| telnyx_failed(&err, "recordings"))?;
                    found = recordings
                        .into_iter()
                        .find(|(_, status)| status == "in-progress")
                        .map(|(recording, _)| (sid.to_string(), recording));
                    if found.is_some() {
                        break;
                    }
                }
                let (on, recording) = found.ok_or_else(unavailable)?;
                saved_sid = Some(format!("{on}|{recording}"));
                saved_sid.clone().unwrap_or_default()
            };
            let (on, recording) = sid.split_once('|').unwrap_or((call_sid, sid.as_str()));
            let status = if input.recording {
                "in-progress"
            } else {
                "paused"
            };
            state
                .telnyx
                .texml_set_recording(account, on, recording, status)
                .await
                .map_err(|err| telnyx_failed(&err, "recording"))?;
        }
    }
    sqlx::query(
        "UPDATE phone_calls
            SET live_recording = ?, live_recording_sid = COALESCE(?, live_recording_sid)
          WHERE id = ? AND company_id = ?",
    )
    .bind(if input.recording {
        "recording"
    } else {
        "paused"
    })
    .bind(saved_sid)
    .bind(call.id)
    .bind(user.company_id)
    .execute(pool)
    .await?;
    Ok(Json(LiveRecording {
        call_id: call.id,
        recording: input.recording,
    }))
}
