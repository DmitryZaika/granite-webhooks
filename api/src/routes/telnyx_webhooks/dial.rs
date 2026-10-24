//! An incoming call nobody answered: `dial-result` (the `<Dial>` action and
//! where declines send the call), the cell fallback's "press 1" screen, the
//! AI Assistant's action, and the end of a voicemail.

use super::{
    Hook, TexmlRequest, failed, fallback, hang_up, no_answer, other_party, records_inbound, texml,
    voicemail,
};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::phone::call_log::{self, Answer, CallFacts, End};
use crate::phone::calls::{CallDirection, CallStatus};
use crate::phone::numbers::last10;
use crate::phone::outcomes::{self, Push};
use crate::phone::routing::{self, DEFAULT_GREETING, NoAnswer};
use crate::phone::settings;
use crate::state::AppState;
use crate::telnyx::{Response as Texml, Verb};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use utoipa::IntoParams;

/// Query of `dial-result`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DialParams {
    /// Our call id.
    pub call: i64,
    /// `1` when a user declined the call on a phone
    /// (`POST /v1/phone/calls/decline` sent it here).
    pub declined: Option<String>,
    /// `1` after the cells of users with cell fallback on were rung.
    pub cell: Option<String>,
}

/// Query of the AI Assistant's action and of the voicemail's end.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CallParams {
    /// Our call id.
    pub call: i64,
}

/// Query of the cell fallback's screen.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CellParams {
    /// Our call id.
    pub call: i64,
    /// The user whose cell is ringing.
    pub user: i32,
}

/// Continue a call after its phones rang
///
/// The `<Dial>` action of an incoming call (and where a decline sends it,
/// `declined=1`). Answered (`DialCallStatus=completed`): the call is logged
/// as answered and the TeXML hangs up. The caller hung up: logged as
/// canceled, hang up. Else the next step: the verified cells of the users it
/// rings with cell fallback on (not after a decline, and once), each behind
/// "press 1"; then the number's no-answer action: the AI Assistant
/// (`after-ai` next), voicemail (no assistant id: voicemail too) or a
/// forward. Entering the no-answer action logs the call as `missed` (or
/// `declined`) and pushes `missed_call` to the users it rang. Any internal
/// error answers the company's fallback TeXML (`<Redirect>`).
#[utoipa::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/dial-result",
    operation_id = "continue_telnyx_dial_result",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), DialParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's `<Dial>` action request: `CallSid`, `CallStatus`, `DialCallStatus`, `DialCallSid`, `DialCallDuration`"),
    responses(
        (status = 200, description = "TeXML: hang up, ring the cells, or the no-answer action (or the fallback `<Redirect>`)", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request, or no `call`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn dial_result(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<DialParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "dial-result").await?;
    Ok(match next_step(&state, &hook, &params).await {
        Ok(response) => response,
        Err(err) => {
            failed("dial-result", hook.company_id(), &err);
            fallback(&hook.config, &state.phone)
        }
    })
}

/// The incoming call of the company, or an error (the fallback answers).
async fn inbound_call(state: &AppState, hook: &Hook, call_id: i64) -> ApiResult<CallFacts> {
    call_log::facts(&state.pool, hook.company_id(), call_id)
        .await?
        .filter(|call| call.direction == CallDirection::Inbound)
        .ok_or(ApiError::NotFound)
}

async fn next_step(state: &AppState, hook: &Hook, params: &DialParams) -> ApiResult<Response> {
    let pool = &state.pool;
    let company_id = hook.company_id();
    let declined = params.declined.as_deref() == Some("1");
    let cells_tried = params.cell.as_deref() == Some("1");
    let kind = if declined {
        "dial-result:declined"
    } else if cells_tried {
        "dial-result:cell"
    } else {
        "dial-result"
    };
    let call = inbound_call(state, hook, params.call).await?;
    let claim = hook.claim(state, kind, Some(call.id)).await?;
    let form = &hook.form;
    let connected = !declined
        && form.dial_call_status() == Some("completed")
        // A cell that never pressed 1 was not answered.
        && (!cells_tried || call.answered);
    if connected {
        if claim.new {
            end_answered(state, hook, &call).await?;
        }
        return Ok(hang_up());
    }
    if call.ended || call.answered || (!declined && form.call_status() == Some("completed")) {
        // Over (or the caller hung up while it rang).
        if claim.new
            && let Some(ended) = call_log::finalize(
                pool,
                company_id,
                call.id,
                CallStatus::Canceled,
                None,
                End::at(form.occurred_at()),
            )
            .await?
        {
            outcomes::after_end(state, &ended).await;
        }
        return Ok(hang_up());
    }
    let config = &hook.config;
    let declined = declined || call.status == CallStatus::Declined;
    let line = match call.number_id {
        Some(number_id) => routing::line(pool, company_id, number_id).await?,
        None => None,
    };
    let (step, timeout) = match &line {
        Some(line) => {
            let ringers = routing::ringers(pool, config, line, Utc::now()).await?;
            let step = routing::no_answer(
                line,
                config,
                routing::cells(&ringers),
                cells_tried,
                declined,
            );
            (step, routing::ring_timeout(&ringers))
        }
        None => (
            NoAnswer::Voicemail(DEFAULT_GREETING.to_string()),
            routing::ring_timeout(&[]),
        ),
    };
    if claim.new && !matches!(step, NoAnswer::Cells(_)) {
        let to = if declined {
            CallStatus::Declined
        } else {
            CallStatus::Missed
        };
        if call_log::set_status(pool, company_id, call.id, &[CallStatus::Ringing], to).await? {
            outcomes::notify(state, &call, Push::MissedCall).await;
        }
    }
    Ok(no_answer(
        Texml::new(),
        &step,
        &hook.base,
        call.id,
        timeout,
        records_inbound(config),
    )
    .into_response())
}

/// When a leg that talked `talk_secs` (already held to a day) and ended at
/// `ended` (else now) was answered; `None` when the time cannot be told.
fn answered_at(ended: Option<DateTime<Utc>>, talk_secs: i64) -> Option<DateTime<Utc>> {
    ended
        .unwrap_or_else(Utc::now)
        .checked_sub_signed(Duration::try_seconds(talk_secs)?)
}

/// Ends a call that was answered and is over (`DialCallStatus=completed`).
async fn end_answered(state: &AppState, hook: &Hook, call: &CallFacts) -> ApiResult<()> {
    let pool = &state.pool;
    let company_id = hook.company_id();
    let form = &hook.form;
    let talk_secs = form.dial_call_duration();
    if !call.answered {
        // Telnyx's leg callback has not come (yet): the leg says who.
        let device_id = match form.dial_call_sid() {
            Some(leg) => call_log::leg_device(pool, call.id, leg).await?,
            None => None,
        };
        let user_id = match device_id {
            Some(device) => {
                sqlx::query_scalar(
                    "SELECT user_id FROM phone_devices WHERE id = ? AND company_id = ?",
                )
                .bind(device)
                .bind(company_id)
                .fetch_optional(pool)
                .await?
            }
            None => None,
        };
        let at = talk_secs.and_then(|secs| answered_at(form.occurred_at(), secs));
        call_log::answered(
            pool,
            company_id,
            call.id,
            Answer {
                device_id,
                user_id,
                ai_notes: None,
                at,
            },
        )
        .await?;
    }
    let end = End {
        at: form.occurred_at(),
        talk_secs,
    };
    if let Some(ended) =
        call_log::finalize(pool, company_id, call.id, CallStatus::Canceled, None, end).await?
    {
        outcomes::after_end(state, &ended).await;
    }
    Ok(())
}

/// Screen a fallback cell
///
/// The `<Number url>` of a cell rung after the phones: the user hears who is
/// calling and "Press 1 to answer"; anything else (or nothing) hangs the
/// cell up, so its voicemail cannot take the call. The answer goes to
/// `cell-screen-result`. A call answered meanwhile hangs the cell up.
#[utoipa::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/cell-screen",
    operation_id = "screen_telnyx_fallback_cell",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), CellParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's `<Number url>` request for the cell's leg: `CallSid`, `To`"),
    responses(
        (status = 200, description = "TeXML: \"Press 1 to answer\", or hang up", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request, or no `call` / `user`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn cell_screen(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<CellParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "cell-screen").await?;
    Ok(match screen(&state, &hook, &params).await {
        Ok(response) => response,
        Err(err) => {
            failed("cell-screen", hook.company_id(), &err);
            hang_up()
        }
    })
}

async fn screen(state: &AppState, hook: &Hook, params: &CellParams) -> ApiResult<Response> {
    let call = inbound_call(state, hook, params.call).await?;
    hook.claim(state, "cell-screen", Some(call.id)).await?;
    if call.answered || call.ended {
        return Ok(hang_up());
    }
    let who = other_party(state, &call).await?;
    let gather = Verb::new("Gather")
        .attr("numDigits", "1")
        .attr("timeout", "8")
        .attr(
            "action",
            format!(
                "{}/cell-screen-result?call={}&user={}",
                hook.base, call.id, params.user
            ),
        )
        .child(Verb::new("Say").text(format!(
            "Granite Phone: a call from {who}. Press 1 to answer."
        )));
    Ok(Texml::new().verb(gather).hangup().into_response())
}

/// Answer on a fallback cell
///
/// The cell screen's `<Gather>` action: `Digits=1` connects the caller to
/// this cell (the call is logged as answered by the cell's user, when no one
/// answered first); anything else hangs the cell up, as does a leg whose
/// `To` is not that user's verified cell.
#[utoipa::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/cell-screen-result",
    operation_id = "answer_telnyx_fallback_cell",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), CellParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's `<Gather>` action request: `CallSid`, `To` (the cell), `Digits`"),
    responses(
        (status = 200, description = "TeXML: an empty answer (connect) or hang up", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request, or no `call` / `user`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn cell_screen_result(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<CellParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "cell-screen-result").await?;
    Ok(match take_on_cell(&state, &hook, &params).await {
        Ok(response) => response,
        Err(err) => {
            failed("cell-screen-result", hook.company_id(), &err);
            hang_up()
        }
    })
}

async fn take_on_cell(state: &AppState, hook: &Hook, params: &CellParams) -> ApiResult<Response> {
    let pool = &state.pool;
    let company_id = hook.company_id();
    let call = inbound_call(state, hook, params.call).await?;
    hook.claim(state, "cell-screen-result", Some(call.id))
        .await?;
    if hook.form.digits() != Some("1") || call.ended {
        return Ok(hang_up());
    }
    if call.answered {
        // A repeat of this user's answer connects again; anyone else is late.
        return Ok(if call.user_id == Some(params.user) {
            Texml::new().into_response()
        } else {
            hang_up()
        });
    }
    let user: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
    )
    .bind(params.user)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let Some(user_id) = user else {
        return Ok(hang_up());
    };
    let stored = settings::load(pool, user_id, company_id).await?;
    // The leg must be the one rung on that user's verified cell.
    let to_their_cell = hook.form.to().and_then(last10).is_some_and(|called| {
        stored.cell_number.as_deref().and_then(last10).as_ref() == Some(&called)
    });
    if !to_their_cell {
        return Ok(hang_up());
    }
    let ai_notes = stored.ai_notes;
    let won = call_log::answered(
        pool,
        company_id,
        call.id,
        Answer {
            device_id: None,
            user_id: Some(user_id),
            ai_notes: Some(ai_notes),
            at: None,
        },
    )
    .await?;
    Ok(if won {
        Texml::new().into_response()
    } else {
        hang_up()
    })
}

/// Continue after the AI Assistant
///
/// The `<Connect>` action of the AI Assistant. When it could not start
/// (`CallStatus=conversation_start_failed`) the caller leaves a voicemail
/// instead (the number's greeting, then `<Record>`). Otherwise the call is
/// logged as `ai_message`, the users it rang get an `ai_message` push, and
/// the TeXML hangs up. Any internal error answers the fallback TeXML.
#[utoipa::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/after-ai",
    operation_id = "continue_telnyx_after_ai",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), CallParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's `<Connect>` action request: `CallSid`, `CallStatus`, `Reason`"),
    responses(
        (status = 200, description = "TeXML: voicemail, or hang up (or the fallback `<Redirect>`)", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request, or no `call`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn after_ai(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<CallParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "after-ai").await?;
    Ok(match after_assistant(&state, &hook, params.call).await {
        Ok(response) => response,
        Err(err) => {
            failed("after-ai", hook.company_id(), &err);
            fallback(&hook.config, &state.phone)
        }
    })
}

/// Statuses a call may have before a message replaces them.
const BEFORE_MESSAGE: [CallStatus; 4] = [
    CallStatus::Ringing,
    CallStatus::Missed,
    CallStatus::Declined,
    CallStatus::Canceled,
];

async fn after_assistant(state: &AppState, hook: &Hook, call_id: i64) -> ApiResult<Response> {
    let pool = &state.pool;
    let company_id = hook.company_id();
    let call = inbound_call(state, hook, call_id).await?;
    let claim = hook.claim(state, "after-ai", Some(call.id)).await?;
    if hook.form.call_status() == Some("conversation_start_failed") {
        let greeting = match call.number_id {
            Some(number_id) => routing::line(pool, company_id, number_id)
                .await?
                .and_then(|line| line.greeting)
                .map(|greeting| greeting.trim().to_string())
                .filter(|greeting| !greeting.is_empty()),
            None => None,
        };
        let greeting = greeting.as_deref().unwrap_or(DEFAULT_GREETING);
        return Ok(voicemail(Texml::new(), greeting, &hook.base, call.id).into_response());
    }
    if claim.new {
        if call_log::set_status(
            pool,
            company_id,
            call.id,
            &BEFORE_MESSAGE,
            CallStatus::AiMessage,
        )
        .await?
        {
            outcomes::notify(state, &call, Push::AiMessage).await;
        }
        if let Some(ended) = call_log::finalize(
            pool,
            company_id,
            call.id,
            CallStatus::Missed,
            None,
            End::at(hook.form.occurred_at()),
        )
        .await?
        {
            outcomes::after_end(state, &ended).await;
        }
    }
    Ok(hang_up())
}

/// End a voicemail
///
/// The `<Record>` action of a voicemail: the call is logged as `voicemail`
/// (unless nothing was recorded) and ends, the users it rang get a
/// `voicemail` push, and the TeXML hangs up. The recording itself arrives
/// at `recording?kind=voicemail`.
#[utoipa::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/voicemail-done",
    operation_id = "finish_telnyx_voicemail",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), CallParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's `<Record>` action request: `CallSid`, `RecordingSid`, `RecordingDuration`"),
    responses(
        (status = 200, description = "TeXML: hang up", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request, or no `call`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn voicemail_done(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<CallParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "voicemail-done").await?;
    if let Err(err) = end_voicemail(&state, &hook, params.call).await {
        failed("voicemail-done", hook.company_id(), &err);
    }
    Ok(hang_up())
}

async fn end_voicemail(state: &AppState, hook: &Hook, call_id: i64) -> ApiResult<()> {
    let pool = &state.pool;
    let company_id = hook.company_id();
    let call = inbound_call(state, hook, call_id).await?;
    let claim = hook.claim(state, "voicemail-done", Some(call.id)).await?;
    if !claim.new {
        return Ok(());
    }
    let recorded = hook.form.recording_duration().is_none_or(|secs| secs > 0);
    if recorded
        && call_log::set_status(
            pool,
            company_id,
            call.id,
            &BEFORE_MESSAGE,
            CallStatus::Voicemail,
        )
        .await?
    {
        outcomes::notify(state, &call, Push::Voicemail).await;
    }
    if let Some(ended) = call_log::finalize(
        pool,
        company_id,
        call.id,
        CallStatus::Missed,
        None,
        End::at(hook.form.occurred_at()),
    )
    .await?
    {
        outcomes::after_end(state, &ended).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_told_back_from_the_end_without_overflow() {
        let ended = DateTime::parse_from_rfc3339("2026-10-18T10:05:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            answered_at(Some(ended), 300).map(|at| at.to_rfc3339()),
            Some("2026-10-18T10:00:00+00:00".to_string())
        );
        // Out of range: no time, never a panic.
        assert_eq!(answered_at(Some(ended), i64::MAX), None);
        assert_eq!(answered_at(Some(DateTime::<Utc>::MIN_UTC), 1), None);
    }
}
