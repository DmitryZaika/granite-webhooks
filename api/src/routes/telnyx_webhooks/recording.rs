//! `POST /v1/webhooks/telnyx/{token}/recording`: a call's recording (or a
//! voicemail) is ready on Telnyx.

use super::{TexmlRequest, taken, texml};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::phone::call_log;
use crate::phone::events;
use crate::phone::recordings::{self, RecordingKind};
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use serde::Deserialize;
use utoipa::IntoParams;

/// Query of `recording`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RecordingParams {
    /// Our call id.
    pub call: i64,
    /// `call` (the conversation; default) or `voicemail`.
    pub kind: Option<String>,
}

/// Copy a call's recording
///
/// The `recordingStatusCallback` of a recorded `<Dial>` (`kind=call`) or a
/// voicemail's `<Record>` (`kind=voicemail`). When Telnyx says the recording
/// is `completed`, it is looked up by `RecordingSid` with our key (never
/// the callback's link) and must be of this call (Telnyx's call ids for it
/// include one of the call's, its connection is the company's; else
/// nothing is stored or deleted), downloaded, stored once in the private
/// recordings bucket at `phone/{company}/{yyyy}/{mm}/{call id}-{kind}.mp3`
/// and checked (our copy's size must match); then Telnyx's copy is deleted
/// (retried by the phone job when that fails). The call gets
/// `hasRecording`, and is queued for transcription (`transcriptStatus`
/// `pending`) when the call's user has AI notes on, for any voicemail, and
/// for an answered outgoing call (its transcript scores it; its AI note
/// still needs AI notes on). Each copy
/// counts as an attempt; one that fails leaves the recording pending for
/// the phone job to retry. Repeats and other statuses change nothing.
#[utoipa::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/recording",
    operation_id = "copy_telnyx_recording",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), RecordingParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's recording status callback: `CallSid`, `RecordingSid`, `RecordingStatus`, `RecordingDuration`"),
    responses(
        (status = 200, description = "Taken (copied, or left for the phone job)"),
        (status = 400, description = "Not a form-encoded request, no `call`, or an unknown `kind`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error (Telnyx sends it again)", body = ErrorBody),
    )
)]
pub async fn recording(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<RecordingParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "recording").await?;
    let kind = match params.kind.as_deref() {
        None => RecordingKind::Call,
        Some(kind) => RecordingKind::parse(kind)
            .ok_or_else(|| ApiError::BadRequest("kind must be call or voicemail".into()))?,
    };
    // Each recording status is its own event.
    let event = format!(
        "recording:{}:{}",
        kind.as_str(),
        hook.form.recording_status().unwrap_or("unknown")
    );
    let pool = &state.pool;
    let call = call_log::facts(pool, hook.company_id(), params.call).await?;
    let claim = hook
        .claim(&state, &event, call.as_ref().map(|call| call.id))
        .await?;
    let (Some(call), Some(recording_sid), true, Some("completed")) = (
        call,
        hook.form.recording_sid(),
        claim.new,
        hook.form.recording_status(),
    ) else {
        return Ok(taken());
    };
    let copied = recordings::copy(
        &state,
        hook.company_id(),
        call.id,
        recording_sid,
        kind,
        hook.form.recording_duration(),
    )
    .await;
    if let Err(err) = copied {
        events::release(pool, &claim.key).await?;
        return Err(err.into());
    }
    Ok(taken())
}
