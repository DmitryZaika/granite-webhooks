//! `GET /v1/calls/{provider}/{media_id}/recording`: where the browser can
//! load a call's audio.
//!
//! The audio is copied from the provider into the bucket on first listen
//! (recordings are often over Lambda's 6 MB response limit) and answered as
//! a presigned link the player fetches. Replaces the Remix
//! `api.{cloudtalk,ringcentral}.userCallMedia.$callId` routes, which
//! streamed it.

use super::recording::{CallTranscriptError, MediaToken, fetch_audio};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::routes::sms::provider::SmsProvider;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use hmac::{Hmac, KeyInit, Mac};
use lambda_http::tracing;
use serde::Serialize;
use sha2::Sha256;
use std::fmt::Write as _;
use utoipa::ToSchema;

/// Where to load a call's audio.
#[derive(Debug, Serialize, ToSchema)]
pub struct CallRecording {
    /// A link to the audio, valid an hour.
    pub url: String,
    /// Its type, e.g. `audio/mpeg` or `audio/wav`.
    pub content_type: String,
}

/// The audio's key: not guessable from the call, since the bucket's files
/// are public.
fn cache_key(secret: &str, company_id: i32, provider: SmsProvider, media_id: &str) -> String {
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
#[utoipa::path(
    get,
    path = "/v1/calls/{provider}/{media_id}/recording",
    operation_id = "get_call_recording",
    tag = "calls",
    params(
        ("provider" = String, Path, description = "`cloudtalk` or `ringcentral`"),
        ("media_id" = String, Path, description = "CloudTalk call id or RingCentral media token"),
    ),
    responses(
        (status = 200, description = "Where to load the audio", body = CallRecording),
        (status = 400, description = "Not a call id or media token of the provider", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "The call has no recording", body = ErrorBody),
        (status = 502, description = "The provider could not be reached", body = ErrorBody),
        (status = 503, description = "No bucket configured", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn recording(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((provider, media_id)): Path<(SmsProvider, String)>,
) -> ApiResult<Json<CallRecording>> {
    let media_id = media_id.trim();
    let (call_id, link) = match provider {
        SmsProvider::Cloudtalk
            if !media_id.is_empty() && media_id.bytes().all(|b| b.is_ascii_digit()) =>
        {
            (media_id, None)
        }
        SmsProvider::Ringcentral if MediaToken::parse(media_id).is_some() => ("", Some(media_id)),
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
