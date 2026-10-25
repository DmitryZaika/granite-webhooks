//! `GET /v1/calls/{provider}/{media_id}/recording`: where the browser can
//! load a call's audio.
//!
//! The audio is copied from the provider into the bucket on first listen
//! (recordings are often over Lambda's 6 MB response limit) and answered as
//! a presigned link the player fetches. Replaces the Remix
//! `api.{cloudtalk,ringcentral}.userCallMedia.$callId` routes, which
//! streamed it. Granite Phone recordings are ours already: they are played
//! from the private recordings bucket through a 5-minute link.

use super::provider::CallProvider;
use super::recording::{CallTranscriptError, MediaToken, fetch_audio};
use super::telnyx;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use hmac::{Hmac, KeyInit, Mac};
use lambda_http::tracing;
use serde::Serialize;
use sha2::Sha256;
use std::fmt::Write as _;

/// Where to load a call's audio.
#[derive(Debug, Serialize, ToSchema)]
pub struct CallRecording {
    /// A link to the audio, valid an hour (5 minutes for a Granite Phone
    /// recording: ask again when it runs out).
    pub url: String,
    /// Its type, e.g. `audio/mpeg` or `audio/wav`.
    pub content_type: String,
}

/// The audio's key: not guessable from the call, since the bucket's files
/// are public.
fn cache_key(secret: &str, company_id: i32, provider: CallProvider, media_id: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(format!("{company_id}/{}/{media_id}", provider.as_str()).as_bytes());
    let hex = mac
        .finalize()
        .into_bytes()
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    format!("call-recordings/{hex}")
}

/// Load a call's recording
///
/// A link (an hour) to the audio of a recorded call or voicemail: a
/// `CloudTalk` call id, or a `RingCentral` media token (`rec_<id>`,
/// `vm_<extension>_<message>_<attachment>`) as the call lists give them.
/// For `telnyx`, a Granite Phone call `tx_<id>` of the caller's company
/// (`Cdr.recording_link` of `GET /v1/calls/telnyx`): a 5-minute link to our
/// private copy; 404 for another company's call or one not recorded, 403
/// when the caller may not hear it (only the call's user, the users of its
/// direct line, for an unanswered incoming call the users its line rings,
/// its deal's owner, admins and sales managers may).
#[crate::openapi::path(
    get,
    path = "/v1/calls/{provider}/{media_id}/recording",
    operation_id = "get_call_recording",
    tag = "calls",
    params(
        ("provider" = inline(CallProvider), Path, description = "`cloudtalk`, `ringcentral` or `telnyx`"),
        ("media_id" = String, Path, description = "CloudTalk call id, RingCentral media token, or Granite Phone call `tx_<id>`"),
    ),
    responses(
        (status = 200, description = "Where to load the audio", body = CallRecording),
        (status = 400, description = "Not a call id or media token of the provider", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or may not hear this Granite Phone call", body = ErrorBody),
        (status = 404, description = "The call has no recording (or is not the company's)", body = ErrorBody),
        (status = 502, description = "The provider could not be reached", body = ErrorBody),
        (status = 503, description = "No bucket (or no recordings bucket) configured", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn recording(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((provider, media_id)): Path<(CallProvider, String)>,
) -> ApiResult<Json<CallRecording>> {
    let media_id = media_id.trim();
    let (call_id, link) = match provider {
        CallProvider::Cloudtalk
            if !media_id.is_empty() && media_id.bytes().all(|b| b.is_ascii_digit()) =>
        {
            (media_id, None)
        }
        CallProvider::Ringcentral if MediaToken::parse(media_id).is_some() => ("", Some(media_id)),
        CallProvider::Telnyx => {
            let id = telnyx::parse_token(media_id)
                .ok_or_else(|| ApiError::BadRequest("Unsupported media id".into()))?;
            let key = telnyx::recording_key(&state.pool, user.company_id, id)
                .await?
                .ok_or(ApiError::NotFound)?;
            let hearer = crate::phone::calls::Hearer::of(&user);
            if crate::phone::calls::may_hear(&state.pool, user.company_id, hearer, id).await?
                != Some(true)
            {
                return Err(ApiError::Forbidden);
            }
            let link = crate::phone::recordings::link(&state, &key)
                .await?
                .ok_or_else(|| ApiError::Unavailable("Recordings are not set up.".into()))?;
            return Ok(Json(CallRecording {
                url: link.url,
                content_type: link.content_type.into(),
            }));
        }
        _ => return Err(ApiError::BadRequest("Unsupported media id".into())),
    };
    let key = cache_key(&state.session_secret, user.company_id, provider, media_id);
    let content_type = if let Some(content_type) = state.storage.head(&key).await? {
        content_type
    } else {
        let (bytes, content_type) = fetch_audio(&state, user.company_id, call_id, link)
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "call recording not fetched");
                match err {
                    CallTranscriptError::NotAvailable | CallTranscriptError::Empty => {
                        ApiError::NotFound
                    }
                    other => ApiError::Upstream(other.to_string()),
                }
            })?;
        if bytes.is_empty() {
            return Err(ApiError::NotFound);
        }
        state.storage.put(&key, bytes, &content_type).await?;
        content_type
    };
    let url = state.storage.presign_get(&key, &content_type).await?;
    Ok(Json(CallRecording { url, content_type }))
}
