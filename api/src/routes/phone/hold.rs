//! Hold music: the built-in tracks, uploading my own, and the app telling
//! us when it holds or resumes a live call (`PUT /v1/phone/calls/hold`),
//! so the other party hears my music instead of silence. How the music
//! plays is `phone::hold_music`.

use crate::auth::device::{DeviceUser, PhoneCaller};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::hold_music::{self, MAX_UPLOAD_BYTES, TRACKS};
use crate::phone::numbers::is_nanp;
use crate::phone::settings;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::Utc;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;

/// A built-in hold music track.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HoldTrack {
    /// Its id: send it as `holdMusic` to `PUT /v1/phone/settings`.
    pub id: String,
    /// What to call it, e.g. "Calm piano".
    pub name: String,
    /// The MP3, public (for the preview button).
    pub url: String,
}

/// `GET /v1/phone/hold-music`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HoldMusicOptions {
    /// The built-in tracks, the default (`calm`) first.
    pub tracks: Vec<HoldTrack>,
    /// The largest file `POST /v1/phone/hold-music/upload` takes, in bytes.
    pub max_upload_bytes: u64,
}

/// List the hold music tracks
///
/// The built-in tracks (original music the company may use freely), with a
/// public MP3 link each for the preview, and the upload size limit. My
/// choice is `holdMusic` in `GET /v1/phone/bootstrap` `settings`; change it
/// with `PUT /v1/phone/settings`. Device token only.
#[utoipa::path(
    get,
    path = "/v1/phone/hold-music",
    operation_id = "list_phone_hold_music",
    tag = "phone",
    responses(
        (status = 200, description = "The tracks", body = HoldMusicOptions),
        (status = 401, description = "No valid device token", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn list_hold_music(
    State(state): State<AppState>,
    _device: DeviceUser,
) -> Json<HoldMusicOptions> {
    let api = &state.phone.public_api_url;
    Json(HoldMusicOptions {
        tracks: TRACKS
            .iter()
            .map(|track| HoldTrack {
                id: track.id.to_string(),
                name: track.name.to_string(),
                url: hold_music::track_url(api, track.id),
            })
            .collect(),
        max_upload_bytes: MAX_UPLOAD_BYTES,
    })
}

/// Body of `POST /v1/phone/hold-music/upload`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HoldMusicUploadInput {
    /// The file's name, e.g. `jingle.mp3`.
    pub file_name: String,
    /// Its type: `audio/mpeg` or `audio/wav` (an `.mp3` / `.wav` name is
    /// enough).
    #[serde(default)]
    pub content_type: String,
}

/// `POST /v1/phone/hold-music/upload`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HoldMusicUpload {
    /// `PUT` the file here (straight to storage) with this `contentType`;
    /// valid 15 minutes.
    pub upload_url: String,
    /// The `Content-Type` header the PUT must send.
    pub content_type: String,
    /// The file's URL once put: send it as `holdMusicUrl` (with `holdMusic:
    /// custom`) to `PUT /v1/phone/settings`.
    pub url: String,
}

/// Start uploading my hold music
///
/// Returns a presigned URL to `PUT` an MP3 or WAV file to (at most 5 MB;
/// checked when it is chosen), and the URL to choose it with
/// (`PUT /v1/phone/settings` `holdMusic: custom`, `holdMusicUrl`). 400
/// `invalid_hold_music` for another kind of file; 503 when uploads are not
/// set up on the server. Device token only.
#[utoipa::path(
    post,
    path = "/v1/phone/hold-music/upload",
    operation_id = "start_phone_hold_music_upload",
    tag = "phone",
    request_body = HoldMusicUploadInput,
    responses(
        (status = 201, description = "Where to put the file", body = HoldMusicUpload),
        (status = 400, description = "`invalid_hold_music`: not an MP3 or WAV", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Storage error", body = ErrorBody),
        (status = 503, description = "Uploads are not set up on the server", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn start_hold_music_upload(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<HoldMusicUploadInput>,
) -> ApiResult<(StatusCode, Json<HoldMusicUpload>)> {
    let user = &device.user;
    let kind = hold_music::upload_kind(&input.file_name, &input.content_type)
        .ok_or_else(|| ApiError::BadRequest("invalid_hold_music".into()))?;
    let content_type = if kind == "wav" {
        "audio/wav"
    } else {
        "audio/mpeg"
    };
    let key = format!(
        "{}{}.{kind}",
        hold_music::upload_prefix(user.company_id, user.id),
        uuid::Uuid::new_v4()
    );
    let upload_url = state.storage.presign_put(&key, content_type).await?;
    let url = state.storage.public_url(&key)?;
    Ok((
        StatusCode::CREATED,
        Json(HoldMusicUpload {
            upload_url,
            content_type: content_type.to_string(),
            url,
        }),
    ))
}

/// Body of `PUT /v1/phone/calls/hold`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HoldInput {
    /// True: I just put the call on hold; false: I just resumed it.
    pub held: bool,
    /// The ids Telnyx's SDK knows the call by (its call id, call control
    /// id, call session id, leg id; at most 4 are read), to pick the call
    /// when I have more than one (call waiting, swapping); empty = the
    /// newest.
    #[serde(default)]
    pub telnyx_call_ids: Vec<String>,
}

/// `PUT /v1/phone/calls/hold`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HoldState {
    /// Our call id.
    pub call_id: i64,
    /// The call is on hold now.
    pub on_hold: bool,
    /// This request started my hold music for the other party (false:
    /// silence, because I chose none or Telnyx did not take the music; also
    /// on a resume and on a repeat, which change nothing).
    pub music: bool,
}

/// A live call of the caller, as hold needs it.
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
    on_hold: bool,
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

    /// The Call Control leg to play on, and to which legs of it: the
    /// app's own calls on the app's leg to the opposite (the customer);
    /// calls our TeXML app answered on the caller's leg itself.
    fn music_leg(&self) -> Option<(&str, &'static str)> {
        if self.direction == "inbound" {
            self.telnyx_call_sid.as_deref().map(|leg| (leg, "self"))
        } else {
            self.telnyx_call_control_id
                .as_deref()
                .map(|leg| (leg, "opposite"))
        }
    }
}

/// Put my live call on hold (or resume it)
///
/// The app calls this right after it holds or resumes a call (its own
/// hold, a swap between two calls, answering a waiting call), so the other
/// party hears my hold music (`holdMusic` in my settings) instead of
/// silence: Telnyx plays it in a loop until the call is resumed or ends. An
/// end clears the hold by itself. Saying the same thing twice changes
/// nothing. The call is my answered call that has not ended, the one the
/// given Telnyx ids name (else the newest). The hold itself is the app's:
/// when Telnyx does not take the music the call is still on hold, with
/// `music: false`. 404 when I am on no call; 409 `hold_unavailable` for a
/// Call via my cell call or a test call. Takes the device token or the
/// session cookie.
#[utoipa::path(
    put,
    path = "/v1/phone/calls/hold",
    operation_id = "set_live_call_hold",
    tag = "phone",
    request_body = HoldInput,
    responses(
        (status = 200, description = "On hold (or not) now", body = HoldState),
        (status = 401, description = "No valid device token or session", body = ErrorBody),
        (status = 403, description = "A cookie of a user who is not an employee or admin", body = ErrorBody),
        (status = 404, description = "I am on no call", body = ErrorBody),
        (status = 409, description = "`hold_unavailable`", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []), ("session_cookie" = []))
)]
pub async fn set_live_call_hold(
    State(state): State<AppState>,
    caller: PhoneCaller,
    JsonBody(input): JsonBody<HoldInput>,
) -> ApiResult<Json<HoldState>> {
    let user = &caller.user;
    let pool = &state.pool;
    let calls: Vec<LiveCall> = sqlx::query_as(
        "SELECT CAST(pc.id AS SIGNED) AS id, CAST(pc.direction AS CHAR) AS direction,
                pc.via_cell <> 0 AS via_cell, pc.other_number, pc.our_number,
                pc.telnyx_call_sid, pc.telnyx_call_control_id, pc.telnyx_call_session_id,
                pc.on_hold <> 0 AS on_hold,
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
    let test = call.our_number.as_deref() == Some(call.other_number.as_str());
    if call.via_cell || test || !is_nanp(&call.other_number) {
        return Err(ApiError::Conflict("hold_unavailable".into()));
    }
    if input.held == call.on_hold {
        return Ok(Json(HoldState {
            call_id: call.id,
            on_hold: call.on_hold,
            music: false,
        }));
    }
    let command = format!("hold-{}-{}", call.id, Utc::now().timestamp_millis());
    let mut music = false;
    if let Some((leg, target)) = call.music_leg().filter(|_| state.telnyx.is_enabled()) {
        if input.held {
            let stored = settings::load(pool, user.id, user.company_id).await?;
            let url = hold_music::audio_url(
                &state.phone.public_api_url,
                &stored.hold_music,
                stored.hold_music_url.as_deref(),
            );
            if let Some(url) = url {
                match state.telnyx.play_loop(leg, &url, target, &command).await {
                    Ok(()) => music = true,
                    Err(err) => {
                        tracing::warn!(call_id = call.id, error = %err, "hold music not started");
                    }
                }
            }
        } else if let Err(err) = state.telnyx.stop_playback(leg, &command).await
            && err.code() != Some("90018")
        {
            tracing::warn!(call_id = call.id, error = %err, "hold music not stopped");
        }
    }
    sqlx::query(
        "UPDATE phone_calls
            SET on_hold = ?, held_at = CASE WHEN ? THEN UTC_TIMESTAMP(3) END
          WHERE id = ? AND company_id = ? AND ended_at IS NULL",
    )
    .bind(input.held)
    .bind(input.held)
    .bind(call.id)
    .bind(user.company_id)
    .execute(pool)
    .await?;
    Ok(Json(HoldState {
        call_id: call.id,
        on_hold: input.held,
        music,
    }))
}
