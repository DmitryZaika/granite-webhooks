//! `POST /v1/call-transcripts`: transcribe a recorded call on demand.
//! Remix: `api.aiTranscribe.call.$callId`.

use super::recording::{CallTranscriptError, transcribe_call};
use crate::ai::Caller;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

/// Body of `POST /v1/call-transcripts`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TranscriptInput {
    /// `CloudTalk`'s numeric call id, the `RingCentral` call id, or a
    /// Granite Phone call `tx_<id>` (`Cdr.id` of `GET /v1/calls/telnyx`).
    pub call_id: String,
    /// The call's `recording_link`: a `RingCentral` media token, a
    /// `CloudTalk` URL, or the Granite Phone call `tx_<id>`.
    #[serde(default)]
    pub recording_link: Option<String>,
}

/// A call's transcript.
#[derive(Debug, Serialize, ToSchema)]
pub struct CallTranscript {
    /// What was said.
    pub text: String,
}

/// Transcribe a call
///
/// Downloads the call's recording from the provider and transcribes it.
/// `RingCentral` voicemail reuses `RingCentral`'s own transcript. A Granite
/// Phone call (`tx_<id>`) answers the transcript kept on the call (speaker
/// turns as `Speaker A: ...` lines), else transcribes our copy of its
/// recording and keeps it; 404 when it has no recording or is another
/// company's, 403 (before anything is transcribed) when the caller may not
/// hear it (only the call's user, the users of its direct line, for an
/// unanswered incoming call the users its line rings, its deal's owner,
/// admins and sales managers may).
#[crate::openapi::path(
    post,
    path = "/v1/call-transcripts",
    operation_id = "transcribe_call",
    tag = "calls",
    request_body = TranscriptInput,
    responses(
        (status = 200, description = "Transcript", body = CallTranscript),
        (status = 400, description = "Invalid call id", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or may not hear this Granite Phone call", body = ErrorBody),
        (status = 404, description = "The recording is empty (or a Granite Phone call has none, or is not the company's)", body = ErrorBody),
        (status = 422, description = "No speech in the recording", body = ErrorBody),
        (status = 500, description = "The recording could not be fetched or transcribed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn transcribe(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<TranscriptInput>,
) -> ApiResult<Response> {
    let call_id = input.call_id.trim();
    let valid = !call_id.is_empty()
        && call_id.len() <= 64
        && call_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if !valid {
        return Err(ApiError::BadRequest("Invalid call id".into()));
    }
    if let Some(id) = super::recording::telnyx_call(call_id, input.recording_link.as_deref()) {
        let hearer = crate::phone::calls::Hearer::of(&user);
        match crate::phone::calls::may_hear(&state.pool, user.company_id, hearer, id).await? {
            Some(true) => {}
            Some(false) => return Err(ApiError::Forbidden),
            None => return Err(ApiError::NotFound),
        }
    }
    let caller = Caller {
        feature: "call_transcription",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    match transcribe_call(
        &state,
        caller,
        call_id,
        input.recording_link.as_deref(),
        false,
    )
    .await
    {
        Ok(transcript) => Ok(Json(CallTranscript {
            text: transcript.text,
        })
        .into_response()),
        Err(err) => {
            let ours = super::telnyx::parse_token(call_id).is_some();
            let status = match err {
                CallTranscriptError::Empty => StatusCode::NOT_FOUND,
                // Our own call log: no such call, or no recording.
                CallTranscriptError::NotAvailable if ours => StatusCode::NOT_FOUND,
                CallTranscriptError::NoSpeech => StatusCode::UNPROCESSABLE_ENTITY,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            Ok((
                status,
                Json(ErrorBody {
                    error: err.user_message(),
                }),
            )
                .into_response())
        }
    }
}
