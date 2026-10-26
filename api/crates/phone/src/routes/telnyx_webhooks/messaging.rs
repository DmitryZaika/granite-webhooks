//! `POST /v1/webhooks/telnyx/{token}/messaging`: the events of the company's
//! Telnyx messaging profile (inbound texts) and of the texts we send
//! (delivery status). See `crate::phone::texting`.

use super::{company_for, refused, taken, verify};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::phone::events;
use crate::phone::texting;
use crate::state::AppState;
use crate::telnyx::messaging::MessagingWebhook;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chrono::Utc;
use lambda_http::tracing;

/// A status about a text we do not know yet is retried by Telnyx (the send
/// may still be recording its id) when it is this recent, in seconds.
const UNKNOWN_STATUS_RETRY_SECS: i64 = 120;

/// Take a Telnyx messaging event
///
/// The webhook of the company's Telnyx messaging profile (its webhook URL is
/// `{webhookBase}/messaging`), also named on every text we send.
/// `message.received`: an inbound text or MMS to one of the company's
/// numbers is stored once (Telnyx's message id), its files are copied to
/// our bucket at once (Telnyx's links expire), and the first time: STOP /
/// START mark the phone opted out / in, the customer's SMS flows stop and
/// their deal leaves the first stage of its pipeline, as with the other
/// providers. A text to a number that is not the company's is ignored.
/// `message.sent` / `message.finalized`: the delivery status of a text we
/// sent (`delivered`, or failed with Telnyx's code; `40300` marks the phone
/// opted out). Other events are taken and ignored. Ed25519-signed,
/// always required. Answers 200; Telnyx sends it again on any other status.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/messaging",
    operation_id = "take_telnyx_messaging_event",
    tag = "telnyx-webhooks",
    params(
        ("token" = String, Path, description = "The company's webhook token"),
        ("telnyx-signature-ed25519" = Option<String>, Header, description = "Telnyx's Ed25519 signature of `{telnyx-timestamp}|{body}` (base64); required"),
        ("telnyx-timestamp" = Option<String>, Header, description = "When Telnyx signed it (Unix seconds; at most 5 minutes off)"),
    ),
    request_body(content = Object, description = "A messaging event, as Telnyx sends it: `data.event_type` (`message.received`, `message.sent`, `message.finalized`), `data.id`, `data.occurred_at`, `data.payload` (`id`, `direction`, `from.phone_number`, `to[].phone_number` and `.status`, `text`, `media[]` with `url` and `content_type`, `errors[].code`, `messaging_profile_id`)"),
    responses(
        (status = 200, description = "Taken"),
        (status = 400, description = "Not a messaging event", body = ErrorBody),
        (status = 401, description = "No valid Telnyx signature", body = ErrorBody),
        (status = 403, description = "The messaging profile is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 409, description = "A status about a text whose send is still being recorded (Telnyx sends it again)", body = ErrorBody),
        (status = 503, description = "Telnyx signatures are not set up on the server (`TELNYX_PUBLIC_KEY`)", body = ErrorBody),
        (status = 500, description = "Database error (Telnyx sends it again)", body = ErrorBody),
    )
)]
pub async fn messaging(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let config = company_for(&state, &token).await?;
    verify(&state, &headers, &body, true)?;
    let hook: MessagingWebhook = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("Not a Telnyx messaging event".into()))?;
    let event = hook.data;
    if let (Some(ours), Some(theirs)) = (
        config.messaging_profile_id.as_deref(),
        event.payload.messaging_profile_id.as_deref(),
    ) && ours != theirs
    {
        return Err(refused(&config, "messaging", "messaging profile"));
    }
    let occurred_at = events::parse_time(event.occurred_at.as_deref());
    match event.event_type.as_str() {
        "message.received" => {
            let received =
                texting::receive(&state, config.company_id, &event.payload, occurred_at).await;
            if let Err(err) = &received {
                super::failed("messaging", config.company_id, err);
            }
            received?;
        }
        "message.sent" | "message.finalized" => {
            let ours = texting::record_status(
                &state.pool,
                config.company_id,
                &event.event_type,
                &event.payload,
                occurred_at,
            )
            .await?;
            let recent = occurred_at
                .is_none_or(|at| (Utc::now() - at).num_seconds() < UNKNOWN_STATUS_RETRY_SECS);
            if !ours && recent {
                tracing::info!(
                    company_id = config.company_id,
                    "Telnyx status of a text not recorded yet: asked again"
                );
                return Err(ApiError::Conflict("unknown_message".into()));
            }
        }
        _ => {}
    }
    Ok(taken())
}
