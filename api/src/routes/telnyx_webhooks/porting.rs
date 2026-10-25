//! `POST /v1/webhooks/telnyx/{token}/porting`: Telnyx's port-in
//! notifications for the company's port orders (each order's `webhook_url`
//! names this route): status changes, new comments, splits, messaging
//! changes and deleted drafts.
//!
//! Signed like Call Control (Ed25519, always required; without
//! `TELNYX_PUBLIC_KEY` refused unless `PHONE_ALLOW_UNSIGNED_WEBHOOKS=1`). A
//! notification is only a nudge: the order is read again from Telnyx and
//! that answer is applied, so what a body says about a status is never
//! believed. The order must be the token's company's (others are ignored,
//! logged with ids only). Each notification is taken once (by its event
//! id); when it cannot be handled (Telnyx unreachable) it is given back and
//! answered 500, so Telnyx sends it again, and the phone job catches up
//! anyway.

use super::{company_for, taken, verify};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::phone::company::CompanyConfig;
use crate::phone::porting::{self as port, Notice, PortRow, PortStatus, RefreshError};
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use lambda_http::tracing;
use serde_json::Value;

/// A Telnyx porting notification. Most come as `{"data": <event>, "meta":
/// {...}}`, some as the event itself.
#[allow(dead_code)] // documents the body; the route reads it as JSON
#[derive(Debug, ToSchema)]
pub struct PortingNotification {
    /// The event: `id`, `event_type` (`porting_order.status_changed`,
    /// `porting_order.new_comment`, `porting_order.split`,
    /// `porting_order.messaging_changed`, `porting_order.deleted`, ...),
    /// `occurred_at` and `payload` (the order's `id`, or `porting_order_id`
    /// and the `comment` with its `user_type`; a split's `from.id` and
    /// `to.id`).
    #[schema(value_type = Object)]
    pub data: Value,
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Take a Telnyx porting notification
///
/// Telnyx's notification about one of the company's port orders. The
/// order is read again from Telnyx and our copy brought up to date (a
/// status change is written to its timeline and told to the admins once; a
/// port that completed gets its numbers added to the company). A comment
/// from Telnyx's porting team is noted on the timeline and told to the
/// admins; an order split off ours is saved as the company's too; a deleted
/// draft is marked deleted. Notifications about orders that are not the
/// company's, or seen before, are answered 200 and change nothing.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/porting",
    operation_id = "receive_telnyx_porting_event",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token")),
    request_body(content = PortingNotification, content_type = "application/json", description = "Telnyx's porting notification"),
    responses(
        (status = 200, description = "Taken (or not the company's, or seen before: nothing to do)"),
        (status = 400, description = "Not JSON", body = ErrorBody),
        (status = 401, description = "Telnyx's signature is missing or does not check out", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "It could not be handled (database, or Telnyx unreachable): Telnyx sends it again", body = ErrorBody),
        (status = 503, description = "Telnyx signatures are not set up on the server", body = ErrorBody),
    )
)]
pub async fn porting(
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
    let order_id = match event_type.as_str() {
        "porting_order.new_comment" => text(&payload["porting_order_id"]),
        "porting_order.split" => text(&payload["from"]["id"]),
        _ => text(&payload["id"]).or_else(|| text(&payload["porting_order_id"])),
    };
    let Some(order_id) = order_id else {
        return Ok(taken());
    };
    let Some(row) = port::load_by_telnyx(&state.pool, company_id, &order_id).await? else {
        tracing::warn!(
            company_id,
            event_type,
            "porting notification about an order that is not the company's; ignored"
        );
        return Ok(taken());
    };
    let Some(key) = port::claim_notification(
        &state.pool,
        company_id,
        Some(row.id),
        text(&event["id"]).as_deref(),
        &event_type,
    )
    .await?
    else {
        return Ok(taken());
    };
    match handle(&state, &config, &row, &event_type, payload).await {
        Ok(()) => Ok(taken()),
        Err(err) => {
            tracing::error!(error = %err, company_id, order_id = row.id, event_type, "porting notification failed");
            if let Err(err) = port::release_notification(&state.pool, &key).await {
                tracing::error!(error = %err, "porting notification not given back");
            }
            Err(ApiError::Internal(
                "The notification could not be handled; send it again.".into(),
            ))
        }
    }
}

async fn handle(
    state: &AppState,
    config: &CompanyConfig,
    row: &PortRow,
    event_type: &str,
    payload: &Value,
) -> Result<(), RefreshError> {
    let pool = &state.pool;
    match event_type {
        "porting_order.new_comment" => {
            let from = match payload["comment"]["user_type"].as_str() {
                Some("admin") => "telnyx",
                Some("user") => return Ok(()),
                _ => "system",
            };
            port::record(
                pool,
                row.company_id,
                Some(row.id),
                "comment",
                None,
                Some(from),
                None,
            )
            .await?;
            if from == "telnyx" {
                port::notify(state, row, Notice::Comment).await?;
            }
            Ok(())
        }
        "porting_order.deleted" => {
            port::mark_deleted(state, row, None).await?;
            Ok(())
        }
        "porting_order.split" => {
            if let Some(to) = text(&payload["to"]["id"])
                && port::load_by_telnyx(pool, row.company_id, &to)
                    .await?
                    .is_none()
            {
                split_off(state, config, row, &to).await?;
            }
            port::refresh(state, row).await?;
            Ok(())
        }
        _ => {
            port::refresh(state, row).await?;
            Ok(())
        }
    }
}

/// Saves the order Telnyx split off `row` as the company's (same request,
/// label and starter) and sets it up like ours.
async fn split_off(
    state: &AppState,
    config: &CompanyConfig,
    row: &PortRow,
    telnyx_order_id: &str,
) -> Result<(), RefreshError> {
    let pool = &state.pool;
    let order = state.telnyx.get_porting_order(telnyx_order_id).await?;
    let id = port::insert(
        pool,
        row.company_id,
        &row.request_id,
        &row.label,
        row.created_by_user_id,
        &order,
    )
    .await?;
    let status = PortStatus::parse(&order.status);
    port::record(
        pool,
        row.company_id,
        Some(id),
        "split",
        status,
        Some("from"),
        None,
    )
    .await?;
    port::record(
        pool,
        row.company_id,
        Some(row.id),
        "split",
        None,
        Some("to"),
        None,
    )
    .await?;
    if config.texml_app_id.is_some() && status.is_some_and(PortStatus::editable) {
        let profile = port::messaging_profile_id(pool, row.company_id).await?;
        let changes = port::setup_changes(
            config,
            &port::webhook_url(state, config),
            profile.as_deref(),
            order.messaging_capable,
        );
        if let Err(err) = state
            .telnyx
            .update_porting_order(telnyx_order_id, &changes)
            .await
        {
            tracing::warn!(error = %err, order_id = id, "split port order not set up on Telnyx yet");
            port::set_error(
                pool,
                id,
                "Telnyx did not take the order's setup yet; it is sent again with the next change.",
            )
            .await?;
        }
    }
    Ok(())
}
