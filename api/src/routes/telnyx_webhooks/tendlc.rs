//! `POST /v1/webhooks/telnyx/{token}/10dlc`: Telnyx's 10DLC notifications
//! about the company's brand, campaign and number assignments (the brand's
//! and campaign's `webhookURL` name this route).
//!
//! Signed like Call Control (Ed25519, always required; without
//! `TELNYX_PUBLIC_KEY` refused unless `PHONE_ALLOW_UNSIGNED_WEBHOOKS=1`). A
//! brand or campaign notification is mostly a nudge: the registration is
//! read again from Telnyx and that answer is applied, so what a body says
//! about a status is never believed. What only the body carries is kept:
//! the reasons a campaign was refused, and each number's assignment. It
//! must be about the token's company's brand or campaign (others are
//! ignored, logged with ids only). Each notification is taken once (by its
//! event id); when it cannot be handled (Telnyx unreachable) it is given
//! back and answered 500, so Telnyx sends it again, and the phone job
//! catches up anyway.

use super::{company_for, taken, verify};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::phone::numbers::to_e164;
use crate::phone::tendlc::{self, Row};
use crate::phone::tendlc_registry::{
    RegistryError, add_campaign_feedback, claim_event, number_status, payload_feedback, refresh,
    release_event,
};
use crate::state::AppState;
use crate::telnyx::tendlc::reason_text;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use lambda_http::tracing;
use serde_json::Value;

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// The notification is about the registration's brand or campaign (Telnyx
/// names a campaign by its id, or by TCR's in a few events).
fn ours(row: &Row, payload: &Value) -> bool {
    let brand = text(&payload["brandId"]);
    let campaign = text(&payload["campaignId"]);
    brand
        .as_deref()
        .is_some_and(|id| row.telnyx_brand_id.as_deref() == Some(id))
        || campaign.as_deref().is_some_and(|id| {
            row.telnyx_campaign_id.as_deref() == Some(id)
                || row.tcr_campaign_id.as_deref() == Some(id)
        })
}

/// Take a Telnyx 10DLC notification
///
/// Telnyx's notification about the company's texting registration: a
/// brand update (registered, refused, vetted), a campaign update (Telnyx's
/// or a carrier's review, suspension, provisioning) or a number's
/// assignment. Brand and campaign notifications read the registration
/// again from Telnyx and move it on (the company is told when it is
/// approved, rejected or suspended; a registered brand gets its campaign);
/// a refused campaign's reasons are kept for the superuser and the company.
/// A number notification updates that number's assignment. Notifications
/// about another brand or campaign, or seen before, are answered 200 and
/// change nothing.
#[utoipa::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/10dlc",
    operation_id = "take_telnyx_10dlc_event",
    tag = "telnyx-webhooks",
    params(
        ("token" = String, Path, description = "The company's webhook token"),
        ("telnyx-signature-ed25519" = Option<String>, Header, description = "Telnyx's Ed25519 signature of `{telnyx-timestamp}|{body}` (base64); required"),
        ("telnyx-timestamp" = Option<String>, Header, description = "When Telnyx signed it (Unix seconds; at most 5 minutes off)"),
    ),
    request_body(content = Object, description = "A 10DLC event, as Telnyx sends it: `data.event_type` (`10dlc.brand.update`, `10dlc.campaign.update`, `10dlc.phone_number.update`), `data.id`, `data.occurred_at`, `data.payload` (`brandId` or `campaignId`, `type` such as `REGISTRATION`, `TELNYX_REVIEW`, `MNO_REVIEW`, `VERIFIED`, `ASSIGNMENT`, `STATUS_UPDATE`; `status`, `description`, `reasons[]` with `description` and `fields`, `phoneNumber`)"),
    responses(
        (status = 200, description = "Taken (or not the company's, or seen before: nothing to do)"),
        (status = 400, description = "Not JSON", body = ErrorBody),
        (status = 401, description = "Telnyx's signature is missing or does not check out", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "It could not be handled (database, or Telnyx unreachable): Telnyx sends it again", body = ErrorBody),
        (status = 503, description = "Telnyx signatures are not set up on the server (`TELNYX_PUBLIC_KEY`)", body = ErrorBody),
    )
)]
pub async fn take_telnyx_10dlc_event(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let config = company_for(&state, &token).await?;
    verify(&state, &headers, &body, true)?;
    let notification: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("Not a Telnyx notification".into()))?;
    let event = if notification["data"].is_object() {
        &notification["data"]
    } else {
        &notification
    };
    let event_type = text(&event["event_type"]).unwrap_or_default();
    let payload = &event["payload"];
    let company_id = config.company_id;
    let pool = &state.pool;
    let row = tendlc::load(pool, company_id)
        .await?
        .filter(|row| ours(row, payload));
    let Some(row) = row else {
        tracing::warn!(
            company_id,
            event_type,
            "10DLC notification about a brand or campaign that is not the company's; ignored"
        );
        return Ok(taken());
    };
    let Some(key) =
        claim_event(pool, company_id, text(&event["id"]).as_deref(), &event_type).await?
    else {
        return Ok(taken());
    };
    match handle(&state, &row, &event_type, payload).await {
        Ok(()) => Ok(taken()),
        Err(err) => {
            tracing::error!(
                kind = err.kind(),
                company_id,
                event_type,
                "10DLC notification failed"
            );
            if let Err(err) = release_event(pool, &key).await {
                tracing::error!(error = %err, "10DLC notification not given back");
            }
            Err(ApiError::Internal(
                "The notification could not be handled; send it again.".into(),
            ))
        }
    }
}

async fn handle(
    state: &AppState,
    row: &Row,
    event_type: &str,
    payload: &Value,
) -> Result<(), RegistryError> {
    let pool = &state.pool;
    if event_type == "10dlc.phone_number.update" {
        let Some(e164) = text(&payload["phoneNumber"]).and_then(|number| to_e164(&number)) else {
            return Ok(());
        };
        let reasons = reason_text(&payload["reasons"]);
        let kind = payload["type"].as_str().unwrap_or_default();
        let status = payload["status"].as_str().unwrap_or_default();
        let (status, error) = match (kind, status) {
            ("ASSIGNMENT", "success") | ("STATUS_UPDATE", "ADDED") => ("ASSIGNED", None),
            ("ASSIGNMENT", _) | ("STATUS_UPDATE", "FAILED") => {
                ("FAILED_ASSIGNMENT", reasons.as_deref())
            }
            ("DELETION", "success") | ("STATUS_UPDATE", "DELETED") => ("DELETED", None),
            ("DELETION", _) => ("FAILED_UNASSIGNMENT", reasons.as_deref()),
            ("STATUS_UPDATE", "PENDING") => ("PENDING_ASSIGNMENT", None),
            _ => return Ok(()),
        };
        number_status(pool, row.company_id, &e164, status, error).await?;
        return Ok(());
    }
    if event_type == "10dlc.campaign.update" {
        let reasons = payload_feedback(payload);
        if !reasons.is_empty() {
            add_campaign_feedback(pool, row, &reasons).await?;
        }
    }
    match refresh(state, row, true).await {
        Ok(_) => Ok(()),
        // Telnyx refused to show it (gone from the account): nothing a retry fixes.
        Err(RegistryError::Refused(_)) => {
            tracing::warn!(
                company_id = row.company_id,
                "10DLC registration not readable"
            );
            Ok(())
        }
        Err(err) => Err(err),
    }
}
