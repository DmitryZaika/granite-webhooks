//! `POST /v1/webhooks/telnyx/{token}/inbound`: the TeXML app's voice URL,
//! fetched when a customer calls one of the company's numbers.

use super::{
    Hook, NOTICE, RingOptions, TexmlRequest, failed, fallback, no_answer, records_inbound, refused,
    ring, texml,
};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::phone::blocking::{self, BlockedAction};
use crate::phone::call_log::{self, NewInbound};
use crate::phone::calls::CallStatus;
use crate::phone::contacts;
use crate::phone::events;
use crate::phone::lines::NumberKind;
use crate::phone::numbers::{last10, to_e164};
use crate::phone::outcomes::{self, Push};
use crate::phone::routing::{self, Line, NoAnswer};
use crate::phone::settings::{self, DisplayNumber};
use crate::state::AppState;
use crate::telnyx::{Response as Texml, Verb};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use chrono::Utc;

/// Answer an incoming call
///
/// Telnyx's TeXML app voice URL, fetched when someone calls one of the
/// company's numbers (`To`). First the company's spam and blocked numbers
/// (`GET /v1/phone/admin/call-blocking`): a trusted number is never
/// blocked; a hidden number (when the company blocks those), a blocked
/// number or area code, or likely spam (STIR/SHAKEN validation failed, or
/// Telnyx's call screening says `SPAM LIKELY`; when the company blocks spam
/// or sends it to voicemail) is logged as blocked and hears the
/// announcement then a hang-up, is rejected, or goes to voicemail without
/// ringing anyone; blocked calls get no missed-call push. Otherwise the
/// call is logged (`ringing`; the customer is the company's newest live
/// customer with the caller's number; `spamLikely` when the caller-ID check
/// says so) and answered with TeXML: "This call may be recorded." when the
/// company records incoming calls, then a `<Dial>` ringing every signed-in
/// phone of the users whose part of the call is to ring (not Do not
/// disturb, no forward of all calls, their first matching incoming call
/// rule, else their working hours; the main line only while the company is
/// open), with `dial-result` as its action, a `leg-status` callback per
/// phone, the `whisper` announcement for users who turned it on, and the
/// number called as the caller ID when every one of them chose to see it.
/// On a user's own direct line their forward, voicemail or message rule
/// takes the whole call (no missed-call push). When no phone can ring it
/// goes straight on: the cells of users with cell fallback on, else the
/// number's no-answer action (AI Assistant, voicemail or forward) and a
/// `missed_call` push. A repeated request answers the same call again. An
/// internal error answers the fallback TeXML (`<Redirect>`). 403 when the
/// number, account or connection is not the token's company's.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/inbound",
    operation_id = "answer_telnyx_inbound_call",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token (the end of `webhookBase`)")),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's TeXML request: `AccountSid`, `CallSid`, `ConnectionId`, `From`, `To`, and when sent `CallerId`, `ShakenStirAttestation`, `ShakenStirValidated`"),
    responses(
        (status = 200, description = "TeXML: ring the phones, a user's rule, the no-answer action, or the blocked-call action", content_type = "application/xml", body = String),
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

/// What the phones show and who hears an announcement, by the settings of
/// the users they ring: the number called instead of the caller's when
/// every one of them wants it (`called`, or the caller hid their number and
/// they chose "show my number instead"); the announcement for each who
/// turned it on for this kind of line.
fn ring_options(line: &Line, ringers: &[routing::Ringer], hidden: bool) -> RingOptions {
    let wants_line = |settings: &settings::UserSettings| {
        settings.display_number == DisplayNumber::Called || (hidden && settings.unknown_show_mine)
    };
    let caller_id = (!ringers.is_empty()
        && ringers.iter().all(|ringer| wants_line(&ringer.settings)))
    .then(|| line.e164.clone());
    let direct = line.kind == NumberKind::Direct;
    let announced = ringers
        .iter()
        .filter(|ringer| {
            if direct {
                ringer.settings.announce_direct
            } else {
                ringer.settings.announce_shared
            }
        })
        .map(|ringer| ringer.user_id)
        .collect();
    RingOptions {
        caller_id,
        announced,
    }
}

/// Logs the call (once) and answers its TeXML.
///
/// The company's blocking comes first: a blocked call is logged apart
/// (`phone_blocked_calls`) and hears the announcement or is hung up, or
/// goes to voicemail without ringing anyone and without a missed-call push.
/// Then the user's own direct line may be taken whole by their forward,
/// voicemail or message (no missed-call push either: they chose it).
#[allow(clippy::too_many_lines)]
async fn answer(state: &AppState, hook: &Hook, line: &Line) -> ApiResult<Response> {
    let pool = &state.pool;
    let config = &hook.config;
    let now = Utc::now();
    let caller = hook.form.from().unwrap_or_default();
    let caller_e164 = to_e164(caller);
    let other_number: String = caller_e164
        .clone()
        .unwrap_or_else(|| caller.chars().take(20).collect());
    let spam = hook.form.spam_likely();
    let telnyx_sid = hook.form.call_sid();
    let blocking = blocking::load(pool, config.company_id).await?;
    let screened = blocking::screen(&blocking, caller_e164.as_deref(), spam);
    if let Some(screened) = &screened {
        blocking::log_blocked(
            pool,
            config.company_id,
            line.id,
            &other_number,
            screened,
            telnyx_sid,
        )
        .await?;
        match screened.action {
            BlockedAction::Announce => {
                return Ok(Texml::new()
                    .say(blocking.announcement())
                    .hangup()
                    .into_response());
            }
            BlockedAction::Hangup => {
                return Ok(Texml::new().verb(Verb::new("Reject")).into_response());
            }
            BlockedAction::Voicemail => {}
        }
    }
    let route = if screened.is_some() {
        routing::Route::default()
    } else {
        routing::route(pool, config, line, caller_e164.as_deref(), now).await?
    };
    let ringers = &route.ringers;
    let users: Vec<i32> = ringers.iter().map(|ringer| ringer.user_id).collect();
    let phones = routing::phones(pool, config.company_id, &users).await?;
    let timeout = routing::ring_timeout(ringers);
    let taken = route
        .takeover
        .as_ref()
        .and_then(|(user_id, decision)| Some((*user_id, routing::takeover_step(line, decision)?)));
    let step = if screened.is_some() {
        Some(NoAnswer::Voicemail(routing::greeting(line)))
    } else if let Some((_, step)) = &taken {
        Some(step.clone())
    } else {
        phones
            .is_empty()
            .then(|| routing::no_answer(line, config, routing::cells(ringers), false, false))
    };
    let status = match step {
        None | Some(NoAnswer::Cells(_)) => CallStatus::Ringing,
        Some(_) => CallStatus::Missed,
    };
    let quiet = screened.is_some() || taken.is_some();
    let other_last10 = last10(&other_number);
    let customer_id = match other_last10.as_deref() {
        Some(digits) => contacts::customer_for_number(pool, config.company_id, digits).await?,
        None => None,
    };
    let deal_id = call_log::open_deal(pool, customer_id).await?;
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
        // Likely spam shows as such; a call the owner's rule took is theirs
        // (their Recents, their voicemail push).
        sqlx::query("UPDATE phone_calls SET spam_likely = ?, user_id = ? WHERE id = ?")
            .bind(spam)
            .bind(taken.as_ref().map(|(user_id, _)| *user_id))
            .bind(id)
            .execute(&mut *tx)
            .await?;
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
    let options = ring_options(line, ringers, caller_e164.is_none());
    response = match &step {
        None => ring(
            response, &phones, timeout, &hook.base, call_id, record, &options,
        ),
        Some(step) => no_answer(response, step, &hook.base, call_id, timeout, record),
    };
    if created
        && status == CallStatus::Missed
        && !quiet
        && let Some(call) = call_log::facts(pool, config.company_id, call_id).await?
    {
        outcomes::notify(state, &call, Push::MissedCall).await;
    }
    Ok(response.into_response())
}
