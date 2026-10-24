//! `POST /v1/webhooks/telnyx/{token}/inbound`: the TeXML app's voice URL,
//! fetched when a customer calls one of the company's numbers.

use super::{
    Hook, NOTICE, TexmlRequest, failed, fallback, no_answer, records_inbound, refused, ring, texml,
};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::phone::call_log::{self, NewInbound};
use crate::phone::calls::CallStatus;
use crate::phone::contacts;
use crate::phone::events;
use crate::phone::numbers::{last10, to_e164};
use crate::phone::outcomes::{self, Push};
use crate::phone::routing::{self, Line, NoAnswer};
use crate::state::AppState;
use crate::telnyx::Response as Texml;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use chrono::Utc;

/// Answer an incoming call
///
/// Telnyx's TeXML app voice URL, fetched when someone calls one of the
/// company's numbers (`To`). Logs the call (`ringing`; the customer is the
/// company's newest live customer with the caller's number) and answers
/// TeXML: "This call may be recorded." when the company records incoming
/// calls, then a `<Dial>` ringing every signed-in phone of the users the
/// number rings who are available now (not Do not disturb, inside their
/// working hours; the main line only while the company is open), with
/// `dial-result` as its action and a `leg-status` callback per phone. When
/// no phone can ring it goes straight on: the cells of users with cell
/// fallback on, else the number's no-answer action (AI Assistant, voicemail
/// or forward) and a `missed_call` push. A repeated request answers the same
/// call again. An internal error answers the fallback TeXML
/// (`<Redirect>`). 403 when the number, account or connection is not the
/// token's company's.
#[utoipa::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/inbound",
    operation_id = "answer_telnyx_inbound_call",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token (the end of `webhookBase`)")),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's TeXML request: `AccountSid`, `CallSid`, `ConnectionId`, `From`, `To`"),
    responses(
        (status = 200, description = "TeXML: ring the phones, or the no-answer action", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The number, account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn inbound(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "inbound").await?;
    let line = match called_line(&state, &hook).await {
        Ok(line) => line,
        Err(ApiError::Forbidden) => return Err(ApiError::Forbidden),
        Err(err) => {
            failed("inbound", hook.company_id(), &err);
            return Ok(fallback(&hook.config, &state.phone));
        }
    };
    Ok(match answer(&state, &hook, &line).await {
        Ok(response) => response,
        Err(err) => {
            failed("inbound", hook.company_id(), &err);
            fallback(&hook.config, &state.phone)
        }
    })
}

/// The number called (`To`): 403 when it is not the company's.
async fn called_line(state: &AppState, hook: &Hook) -> ApiResult<Line> {
    let line = match hook.form.to().and_then(to_e164) {
        Some(e164) => routing::line_by_e164(&state.pool, &e164).await?,
        None => None,
    };
    match line {
        Some(line) if line.company_id == hook.company_id() => Ok(line),
        Some(_) => Err(refused(&hook.config, "inbound", "number")),
        None => Err(refused(&hook.config, "inbound", "unknown number")),
    }
}

/// Logs the call (once) and answers its TeXML.
async fn answer(state: &AppState, hook: &Hook, line: &Line) -> ApiResult<Response> {
    let pool = &state.pool;
    let config = &hook.config;
    let now = Utc::now();
    let ringers = routing::ringers(pool, config, line, now).await?;
    let users: Vec<i32> = ringers.iter().map(|ringer| ringer.user_id).collect();
    let phones = routing::phones(pool, config.company_id, &users).await?;
    let timeout = routing::ring_timeout(&ringers);
    let step = phones
        .is_empty()
        .then(|| routing::no_answer(line, config, routing::cells(&ringers), false, false));
    let status = match step {
        None | Some(NoAnswer::Cells(_)) => CallStatus::Ringing,
        Some(_) => CallStatus::Missed,
    };
    let caller = hook.form.from().unwrap_or_default();
    let other_number: String = to_e164(caller).unwrap_or_else(|| caller.chars().take(20).collect());
    let other_last10 = last10(&other_number);
    let customer_id = match other_last10.as_deref() {
        Some(digits) => contacts::customer_for_number(pool, config.company_id, digits).await?,
        None => None,
    };
    let deal_id = call_log::open_deal(pool, customer_id).await?;
    let telnyx_sid = hook.form.call_sid();
    let key = hook.key("inbound");
    let mut tx = pool.begin().await?;
    let claimed = events::claim(&mut *tx, &hook.event("inbound", &key, None)).await?;
    let existing: Option<i64> = match telnyx_sid {
        Some(sid) => {
            sqlx::query_scalar(
                "SELECT CAST(id AS SIGNED) FROM phone_calls
                  WHERE telnyx_call_sid = ? AND company_id = ? FOR UPDATE",
            )
            .bind(sid)
            .bind(config.company_id)
            .fetch_optional(&mut *tx)
            .await?
        }
        None => None,
    };
    let (call_id, created) = if let Some(id) = existing {
        (id, false)
    } else {
        let id = call_log::insert_inbound(
            &mut tx,
            &NewInbound {
                company_id: config.company_id,
                number_id: line.id,
                our_number: &line.e164,
                other_number: &other_number,
                other_last10: other_last10.as_deref().unwrap_or_default(),
                customer_id,
                deal_id,
                call_sid: telnyx_sid,
                status,
                devices_rung: phones.len(),
                ai_notes: true,
            },
        )
        .await?;
        for phone in &phones {
            call_log::add_queued_leg(&mut tx, id, phone.device_id).await?;
        }
        (id, true)
    };
    if claimed {
        events::link_call(&mut *tx, &key, call_id).await?;
    }
    tx.commit().await?;
    let record = records_inbound(config);
    let mut response = Texml::new();
    if record {
        response = response.say(NOTICE);
    }
    response = match &step {
        None => ring(response, &phones, timeout, &hook.base, call_id, record),
        Some(step) => no_answer(response, step, &hook.base, call_id, timeout, record),
    };
    if created
        && status == CallStatus::Missed
        && let Some(call) = call_log::facts(pool, config.company_id, call_id).await?
    {
        outcomes::notify(state, &call, Push::MissedCall).await;
    }
    Ok(response.into_response())
}
