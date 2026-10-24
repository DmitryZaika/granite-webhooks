//! Status callbacks: a leg of a call (a phone it rings, or the customer
//! dialed by Call via my cell) and the call's own end.

use super::{Hook, TexmlRequest, taken, texml};
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::phone::call_log::{self, Answer, CallFacts, End};
use crate::phone::calls::{CallDirection, CallStatus};
use crate::phone::events;
use crate::phone::outcomes;
use crate::phone::settings;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chrono::Duration;
use lambda_http::tracing;
use serde::Deserialize;
use utoipa::IntoParams;

/// Query of `leg-status`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LegParams {
    /// Our call id.
    pub call: i64,
    /// The phone (`phone_devices.id`) this leg rings; none for the
    /// customer's leg of Call via my cell.
    pub device: Option<i64>,
}

/// Query of `call-status`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CallStatusParams {
    /// Our call id, on the status callbacks of the calls our server places
    /// (Call via my cell, test calls); the TeXML app's own callbacks name
    /// none (the call is found by `CallSid`).
    pub call: Option<i64>,
}

/// Statuses that end a leg.
const FINAL: [&str; 5] = ["completed", "busy", "no-answer", "canceled", "failed"];

/// Record a leg's progress
///
/// The `statusCallback` of each `<Sip>` (a phone an incoming call rings)
/// and of the customer's `<Number>` of Call via my cell: the leg is saved
/// (`initiated`, `ringing`, `in-progress`, ...; a leg never moves back, and
/// a declined one stays declined until it ends). An answered leg makes the
/// call `in_progress`, answered by that phone's user (the first answer
/// wins; `device` counts only when the call rang that phone). When the
/// answering leg ends, so does the call (final status, talk time from the
/// leg's `CallDuration`, else Telnyx's times). The Call via my cell
/// customer's leg `completed` means answered, then ended; one who does not
/// answer logs the call as `busy`, `no_answer`, `canceled` or `failed`.
/// Unknown calls are ignored.
#[utoipa::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/leg-status",
    operation_id = "record_telnyx_leg_status",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), LegParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's status callback for the leg: `CallSid` (the leg), `CallStatus`, `CallDuration`"),
    responses(
        (status = 200, description = "Taken"),
        (status = 400, description = "Not a form-encoded request, or no `call`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error (Telnyx sends it again)", body = ErrorBody),
    )
)]
pub async fn leg_status(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<LegParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "leg-status").await?;
    let pool = &state.pool;
    let Some(call) = call_log::facts(pool, hook.company_id(), params.call).await? else {
        hook.claim(&state, "leg-status", None).await?;
        return Ok(taken());
    };
    let claim = hook.claim(&state, "leg-status", Some(call.id)).await?;
    let Some(leg_sid) = hook.form.call_sid() else {
        return Ok(taken());
    };
    if !claim.new {
        return Ok(taken());
    }
    let status = hook
        .form
        .call_status()
        .unwrap_or("unknown")
        .to_ascii_lowercase();
    if let Err(err) = apply_leg(&state, &hook, &call, params.device, leg_sid, &status).await {
        events::release(pool, &claim.key).await?;
        return Err(err);
    }
    Ok(taken())
}

async fn apply_leg(
    state: &AppState,
    hook: &Hook,
    call: &CallFacts,
    device: Option<i64>,
    leg_sid: &str,
    status: &str,
) -> ApiResult<()> {
    let pool = &state.pool;
    let company_id = hook.company_id();
    let form = &hook.form;
    // The phone, when it is one of the company's and the call rang it.
    let device: Option<(i64, i32)> = match device {
        Some(id) => {
            sqlx::query_as(
                "SELECT CAST(pd.id AS SIGNED), pd.user_id FROM phone_devices pd
                  WHERE pd.id = ? AND pd.company_id = ?
                    AND EXISTS (SELECT 1 FROM phone_call_legs l
                                 WHERE l.call_id = ? AND l.device_id = pd.id)",
            )
            .bind(id)
            .bind(company_id)
            .bind(call.id)
            .fetch_optional(pool)
            .await?
        }
        None => None,
    };
    call_log::record_leg(pool, call.id, device.map(|(id, _)| id), leg_sid, status).await?;
    // The customer's leg of a Call via my cell (it names no phone).
    let customer_leg =
        device.is_none() && call.via_cell && call.direction == CallDirection::Outbound;
    if matches!(status, "in-progress" | "answered") {
        if device.is_none() && !customer_leg {
            // Not a leg this call rang: it answers nothing.
            return Ok(());
        }
        let (device_id, user_id, ai_notes) = match device {
            Some((id, user_id)) => (
                Some(id),
                Some(user_id),
                Some(settings::load(pool, user_id, company_id).await?.ai_notes),
            ),
            None => (None, None, None),
        };
        call_log::answered(
            pool,
            company_id,
            call.id,
            Answer {
                device_id,
                user_id,
                ai_notes,
                at: form.occurred_at(),
            },
        )
        .await?;
        return Ok(());
    }
    if !FINAL.contains(&status) {
        return Ok(());
    }
    let Some(now) = call_log::facts(pool, company_id, call.id).await? else {
        return Ok(());
    };
    let end = End {
        at: form.occurred_at(),
        talk_secs: form.call_duration(),
    };
    if customer_leg && !now.answered && status == "completed" {
        // `completed` follows only an answer: its answer callback was lost or
        // is late. Answered (as long ago as the leg talked), then ended.
        let at = end.at.unwrap_or_else(chrono::Utc::now);
        let answered_at = end
            .talk_secs
            .and_then(Duration::try_seconds)
            .and_then(|talk| at.checked_sub_signed(talk));
        call_log::answered(
            pool,
            company_id,
            call.id,
            Answer {
                at: answered_at.or(Some(at)),
                ..Answer::default()
            },
        )
        .await?;
        finish(state, company_id, call.id, form.hangup_cause(), end).await?;
        return Ok(());
    }
    let answering_leg = now.answered
        && match device {
            Some((id, _)) => now.answered_device_id == Some(id),
            None => customer_leg,
        };
    if answering_leg {
        finish(state, company_id, call.id, form.hangup_cause(), end).await?;
    } else if customer_leg && !now.answered {
        let to = match status {
            "busy" => CallStatus::Busy,
            "failed" => CallStatus::Failed,
            "canceled" => CallStatus::Canceled,
            _ => CallStatus::NoAnswer,
        };
        call_log::set_status(pool, company_id, call.id, &[CallStatus::Ringing], to).await?;
    }
    Ok(())
}

/// Ends an answered call whose answering leg ended, with the follow-through.
async fn finish(
    state: &AppState,
    company_id: i32,
    call_id: i64,
    cause: Option<&str>,
    end: End,
) -> ApiResult<()> {
    if let Some(ended) = call_log::finalize(
        &state.pool,
        company_id,
        call_id,
        CallStatus::Canceled,
        cause,
        end,
    )
    .await?
    {
        outcomes::after_end(state, &ended).await;
    }
    Ok(())
}

/// The call a `call-status` callback is about: ours by `call` (the calls
/// our server places), whose `CallSid` must then be the call's (saved when
/// the call has none yet: the callback can beat Telnyx's answer to our
/// request); else the company's call with the `CallSid`.
async fn status_call(
    state: &AppState,
    hook: &Hook,
    call_id: Option<i64>,
) -> sqlx::Result<Option<CallFacts>> {
    let pool = &state.pool;
    let sid = hook.form.call_sid();
    let Some(call_id) = call_id else {
        return match sid {
            Some(sid) => call_log::by_call_sid(pool, hook.company_id(), sid).await,
            None => Ok(None),
        };
    };
    let Some(call) = call_log::facts(pool, hook.company_id(), call_id).await? else {
        return Ok(None);
    };
    match (call.telnyx_call_sid.as_deref(), sid) {
        (Some(ours), Some(theirs)) if ours == theirs => Ok(Some(call)),
        (None, Some(theirs)) => {
            let saved = sqlx::query(
                "UPDATE phone_calls SET telnyx_call_sid = ?
                  WHERE id = ? AND company_id = ? AND telnyx_call_sid IS NULL",
            )
            .bind(theirs)
            .bind(call.id)
            .bind(call.company_id)
            .execute(pool)
            .await;
            if let Err(err) = saved {
                tracing::warn!(call_id = call.id, error = %err, "call sid not saved");
            }
            Ok(Some(call))
        }
        _ => Ok(None),
    }
}

/// Record a call's end
///
/// The status callback of the TeXML app (by `CallSid`) and of the calls our
/// server places (Call via my cell, test calls: `?call=` names ours; its
/// `CallSid` must be the call's, and is saved when Telnyx's answer to our
/// request had none). A final status (`completed`, `busy`, `no-answer`,
/// `canceled`, `failed`) ends the call once, at Telnyx's time: `answered`
/// with its talk time when someone answered; else an incoming call becomes
/// `canceled` (the caller hung up while it rang), `busy` or `failed`, and an
/// unfinished Call via my cell `canceled` or `failed`; a missed call keeps
/// its status (voicemail, AI message ...). Then the follow-through: a
/// missed-call push when the caller hung up while it rang, and for a
/// customer, the deal leaves its first stage after a talk of 90 seconds or
/// more and the customer's SMS flows stop. Other statuses and unknown calls
/// are only stored.
#[utoipa::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/call-status",
    operation_id = "record_telnyx_call_status",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), CallStatusParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's status callback: `CallSid`, `CallStatus`, `CallDuration`, `HangupCause`, `Timestamp`"),
    responses(
        (status = 200, description = "Taken"),
        (status = 400, description = "Not a form-encoded request", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error (Telnyx sends it again)", body = ErrorBody),
    )
)]
pub async fn call_status(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<CallStatusParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "call-status").await?;
    let pool = &state.pool;
    let call = status_call(&state, &hook, params.call).await?;
    let claim = hook
        .claim(&state, "call-status", call.as_ref().map(|call| call.id))
        .await?;
    let Some(call) = call.filter(|_| claim.new) else {
        return Ok(taken());
    };
    let Some(unanswered) = unanswered_status(&call, hook.form.call_status()) else {
        return Ok(taken());
    };
    match call_log::finalize(
        pool,
        hook.company_id(),
        call.id,
        unanswered,
        hook.form.hangup_cause(),
        End::at(hook.form.occurred_at()),
    )
    .await
    {
        Ok(Some(ended)) => outcomes::after_end(&state, &ended).await,
        Ok(None) => {}
        Err(err) => {
            events::release(pool, &claim.key).await?;
            return Err(err.into());
        }
    }
    Ok(taken())
}

/// What an unanswered call becomes when Telnyx reports `status`; `None`
/// while the call is not over.
fn unanswered_status(call: &CallFacts, status: Option<&str>) -> Option<CallStatus> {
    let status = status?;
    if !FINAL.contains(&status) {
        return None;
    }
    Some(match (call.direction, status) {
        (_, "failed") => CallStatus::Failed,
        (CallDirection::Inbound, "busy") => CallStatus::Busy,
        (CallDirection::Inbound, "no-answer") => CallStatus::Missed,
        // The caller hung up while it rang, or a Call via my cell that never
        // reached the customer.
        _ => CallStatus::Canceled,
    })
}
