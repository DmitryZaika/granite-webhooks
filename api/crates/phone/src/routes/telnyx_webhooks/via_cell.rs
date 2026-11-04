//! Call via my cell (D5): our server rang the user's verified cell
//! (`POST /v1/phone/calls/via-cell`); when they answer they hear "Press 1 to
//! call ...", and 1 dials the customer from the call's caller ID. The
//! customer hears the recording notice when the company records outgoing
//! calls.

use super::{
    Hook, NOTICE, TexmlRequest, failed, hang_up, other_party, recorded, records_outbound, texml,
};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::IntoParams;
use crate::phone::call_log::{self, CallFacts, End};
use crate::phone::calls::{CallDirection, CallStatus};
use crate::phone::{outcomes, settings};
use crate::state::AppState;
use crate::telnyx::{Response as Texml, Verb};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use lambda_http::tracing;
use serde::Deserialize;

/// Query of the Call via my cell callbacks.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ViaCellParams {
    /// Our call id.
    pub call: i64,
}

/// The company's live Call via my cell call.
async fn via_cell_call(state: &AppState, hook: &Hook, call_id: i64) -> ApiResult<CallFacts> {
    call_log::facts(&state.pool, hook.company_id(), call_id)
        .await?
        .filter(|call| call.via_cell && call.direction == CallDirection::Outbound)
        .ok_or(ApiError::NotFound)
}

/// Ask the user's cell to place the call
///
/// The TeXML our Call via my cell call fetches when the user's cell
/// answers: "Press 1 to call {customer's name, or the number digit by
/// digit}." (`<Gather>` one key, 8 seconds, to `via-cell-gather`), else
/// hang up. A call that is over (or not a Call via my cell) hangs up.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/via-cell",
    operation_id = "prompt_telnyx_via_cell",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), ViaCellParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's request when the user's cell answered: `CallSid`, `CallStatus`"),
    responses(
        (status = 200, description = "TeXML: \"Press 1 to call ...\", or hang up", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request, or no `call`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn via_cell(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<ViaCellParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "via-cell").await?;
    Ok(match prompt(&state, &hook, params.call).await {
        Ok(response) => response,
        Err(err) => {
            failed("via-cell", hook.company_id(), &err);
            hang_up()
        }
    })
}

async fn prompt(state: &AppState, hook: &Hook, call_id: i64) -> ApiResult<Response> {
    let call = via_cell_call(state, hook, call_id).await?;
    hook.claim(state, "via-cell", Some(call.id)).await?;
    if call.ended || call.answered {
        return Ok(hang_up());
    }
    if call.telnyx_call_sid.is_none()
        && let Some(sid) = hook.form.call_sid()
    {
        // Telnyx's answer to our request had no sid: its callbacks need it.
        let saved = sqlx::query(
            "UPDATE phone_calls SET telnyx_call_sid = ?
              WHERE id = ? AND company_id = ? AND telnyx_call_sid IS NULL",
        )
        .bind(sid)
        .bind(call.id)
        .bind(call.company_id)
        .execute(&state.pool)
        .await;
        if let Err(err) = saved {
            tracing::warn!(call_id = call.id, error = %err, "call sid not saved");
        }
    }
    let who = other_party(state, &call).await?;
    let gather = Verb::new("Gather")
        .attr("numDigits", "1")
        .attr("timeout", "8")
        .attr(
            "action",
            format!("{}/via-cell-gather?call={}", hook.base, call.id),
        )
        .child(Verb::new("Say").text(format!("Press 1 to call {who}.")));
    Ok(Texml::new().verb(gather).hangup().into_response())
}

/// Dial the customer from the user's cell
///
/// The `<Gather>` action of `via-cell`: `Digits=1` dials the customer from
/// the call's caller ID (`<Dial callerId>`), with a `leg-status` callback
/// on the customer's leg; when the company records outgoing calls the call
/// is recorded and the customer hears "This call may be recorded."
/// (`notice`) first. Any other key ends the call (logged as canceled).
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/via-cell-gather",
    operation_id = "dial_telnyx_via_cell",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), ViaCellParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's `<Gather>` action request: `CallSid`, `Digits`"),
    responses(
        (status = 200, description = "TeXML: `<Dial>` the customer, or hang up", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request, or no `call`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn via_cell_gather(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<ViaCellParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "via-cell-gather").await?;
    Ok(match dial_customer(&state, &hook, params.call).await {
        Ok(response) => response,
        Err(err) => {
            failed("via-cell-gather", hook.company_id(), &err);
            hang_up()
        }
    })
}

async fn dial_customer(state: &AppState, hook: &Hook, call_id: i64) -> ApiResult<Response> {
    let call = via_cell_call(state, hook, call_id).await?;
    let claim = hook.claim(state, "via-cell-gather", Some(call.id)).await?;
    if call.ended {
        return Ok(hang_up());
    }
    let caller_id = call.caller_id.as_deref().or(call.our_number.as_deref());
    let (Some("1"), Some(caller_id)) = (hook.form.digits(), caller_id) else {
        if claim.new
            && let Some(ended) = call_log::finalize(
                &state.pool,
                call.company_id,
                call.id,
                CallStatus::Canceled,
                None,
                End::at(hook.form.occurred_at()),
            )
            .await?
        {
            outcomes::after_end(state, &ended).await;
        }
        return Ok(hang_up());
    };
    let base = &hook.base;
    let record = records_outbound(&hook.config);
    // The user turned the recording voice off (My Account): recorded, no notice.
    let voice = match call.user_id {
        Some(user_id) if record => {
            settings::says_recording_notice(&state.pool, user_id, call.company_id).await
        }
        _ => true,
    };
    let mut customer = Verb::new("Number");
    if record && voice {
        customer = customer.attr("url", format!("{base}/notice"));
    }
    let customer = customer
        .attr(
            "statusCallback",
            format!("{base}/leg-status?call={}", call.id),
        )
        .attr("statusCallbackEvent", "answered completed")
        .text(call.other_number.clone());
    let dial = recorded(
        Verb::new("Dial").attr("callerId", caller_id),
        record,
        base,
        call.id,
    )
    .child(customer);
    Ok(Texml::new().verb(dial).into_response())
}

/// Play the recording notice
///
/// The `<Number url>` the customer's leg of Call via my cell fetches when
/// the company records outgoing calls: "This call may be recorded.", then
/// the call connects.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/notice",
    operation_id = "play_telnyx_recording_notice",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token")),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's `<Number url>` request: `CallSid`"),
    responses(
        (status = 200, description = "TeXML: the notice", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn notice(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "notice").await?;
    if let Err(err) = hook.claim(&state, "notice", None).await {
        failed("notice", hook.company_id(), &err.into());
    }
    Ok(Texml::new().say(NOTICE).into_response())
}
