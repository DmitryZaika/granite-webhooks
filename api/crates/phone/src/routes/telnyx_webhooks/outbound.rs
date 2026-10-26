//! `POST /v1/webhooks/telnyx/{token}/outbound`: Call Control v2 events of
//! the company's credential connection, where the app's calls are parked
//! (`call_parking_enabled`) until we approve them, fail closed.
//!
//! A parked call is approved when: the company's calling is on (and the
//! company not deleted); the phone proves itself (below) and its user is
//! still an employee or admin; the caller ID (`X-Granite-Caller-Id`, else
//! the user's default) is one the user may call from; the number is a
//! callable US or Canada number; and the user is under 6 calls a minute and
//! the company's daily limit (counted in the transaction that logs the
//! call). It is then transferred to the number with that caller ID. When
//! the company records outgoing calls, nobody is recorded without a notice:
//! the company's notice recording plays to the customer when they answer,
//! before the call is bridged and recorded; without one, the call is
//! bridged unrecorded and, once the customer answers, "This call may be
//! recorded." is said to both sides before the recording starts.
//! Anything else is hung up and logged `failed`.
//! Emergency codes (911, 112, 933, 988) are never refused, not even for an
//! unproven phone or with calling off: they are connected and the
//! company's admins alerted (`e911_alert`, deduplicated). The other N11
//! codes (211 ... 811) are refused like any number outside the US and
//! Canada (the app opens the phone's own dialer for them).
//!
//! The phone is the one `X-Granite-Device-Id` names, proven by
//! `X-Granite-Call-Auth`: `v1.<unix s>.<base64url HMAC-SHA256>`, keyed with
//! the phone's call key (the 32 bytes of the `callKey` it got at sign-in,
//! which only that phone holds; derived again here from `SESSION_SECRET`
//! and its device session id, never stored, so a copy of the database
//! cannot sign calls), over
//! `v1|<device id>|<destination E.164>|<caller id>|<unix s>`, and at most
//! 10 minutes off our clock. A missing, wrong or stale signature is
//! refused. When `from` carries a SIP user (`gencred...`), it must be that
//! phone's.

use super::{NOTICE, check_connection, company_for, records_outbound, taken, verify};
use crate::auth::device::call_key;
use crate::auth::session::{SessionUser, effective_user, find_user, user_positions};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::phone::alerts::{self, AlertSource};
use crate::phone::call_log::{self, Answer, CallFacts, End};
use crate::phone::calls::CallStatus;
use crate::phone::company::{self, CompanyConfig};
use crate::phone::devices::LIVE_DEVICE;
use crate::phone::events::{self, Event, Source};
use crate::phone::limits::{self, Daily};
use crate::phone::lines::{self, CallerId, default_caller_id};
use crate::phone::numbers::{emergency_code, is_nanp, last10, to_e164};
use crate::phone::recordings::{self, RecordingKind};
use crate::phone::{clock, contacts, outcomes, settings};
use crate::state::AppState;
use crate::telnyx::Transfer;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use lambda_http::tracing;
use serde::Deserialize;
use sha2::Sha256;
use sqlx::MySqlPool;

/// The sample customer of the onboarding tour, never a real call's.
const SAMPLE_CUSTOMER_ID: i32 = 990_000_021;
/// How long the customer's phone may ring.
const RING_SECS: u32 = 60;
/// How far an app call's `X-Granite-Call-Auth` time may be from our clock.
const CALL_AUTH_WINDOW_SECS: i64 = 600;
/// The customer's leg of an approved call to be recorded after the spoken
/// notice (no notice recording): `spoken-notice`, base64, as Telnyx echoes
/// it in that leg's events.
const SPOKEN_NOTICE_STATE: &str = "c3Bva2VuLW5vdGljZQ==";

/// A Call Control webhook.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Envelope {
    data: Option<EventData>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct EventData {
    id: Option<String>,
    event_type: Option<String>,
    occurred_at: Option<String>,
    payload: Option<Payload>,
}

/// The event fields we read.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Payload {
    call_control_id: Option<String>,
    call_session_id: Option<String>,
    connection_id: Option<String>,
    from: Option<String>,
    to: Option<String>,
    direction: Option<String>,
    state: Option<String>,
    hangup_cause: Option<String>,
    hangup_source: Option<String>,
    recording_id: Option<String>,
    custom_headers: Option<Vec<SipHeader>>,
    /// What our command set on this leg (base64).
    client_state: Option<String>,
}

/// A custom SIP header of the call (the app's `X-Granite-*`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SipHeader {
    #[serde(alias = "header_name")]
    name: Option<String>,
    #[serde(alias = "header_value")]
    value: Option<String>,
}

fn text(value: Option<&String>) -> Option<&str> {
    value
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
}

impl Payload {
    /// The value of custom SIP header `name` (any case).
    fn header(&self, name: &str) -> Option<&str> {
        self.custom_headers
            .iter()
            .flatten()
            .find(|header| {
                header
                    .name
                    .as_deref()
                    .is_some_and(|header| header.trim().eq_ignore_ascii_case(name))
            })
            .and_then(|header| text(header.value.as_ref()))
    }

    fn call_control_id(&self) -> Option<&str> {
        text(self.call_control_id.as_ref())
    }

    fn call_session_id(&self) -> Option<&str> {
        text(self.call_session_id.as_ref())
    }
}

/// Take a Call Control event
///
/// The webhook of the company's credential connection (Call Control v2,
/// JSON). Telnyx's Ed25519 signature is required: without
/// `TELNYX_PUBLIC_KEY` on the server every event is refused (503), unless
/// `PHONE_ALLOW_UNSIGNED_WEBHOOKS=1` (local testing only). `call.initiated`
/// of a parked app call: approved (transferred to the number with the
/// user's caller ID, `command_id` `approve-{call id}`; recorded only after
/// a notice, see the module docs) or hung up and logged `failed`; see the
/// checks in the module docs. Emergency codes (911, 112, 933, 988) are
/// never refused (the admins get an `e911_alert`, at most one per phone in
/// 10 minutes). Later events of the call update it: `call.answered` of the
/// customer's leg (answered, at Telnyx's time; a call recorded after the
/// spoken notice gets it, then its recording; `call.bridged` changes
/// nothing, the customer's phone may still ring),
/// `call.hangup` (final status, talk time, the CRM follow-through),
/// `call.recording.saved` (the recording is copied to the private bucket).
/// Every event is stored and handled once.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/outbound",
    operation_id = "take_telnyx_call_control_event",
    tag = "telnyx-webhooks",
    params(
        ("token" = String, Path, description = "The company's webhook token"),
        ("telnyx-signature-ed25519" = Option<String>, Header, description = "Telnyx's Ed25519 signature of `{telnyx-timestamp}|{body}` (base64); required"),
        ("telnyx-timestamp" = Option<String>, Header, description = "When Telnyx signed it (Unix seconds; at most 5 minutes off)"),
    ),
    request_body(content = Object, description = "A Call Control v2 event, as Telnyx sends it: `data.id`, `data.event_type`, `data.occurred_at`, `data.payload` (`call_control_id`, `call_session_id`, `connection_id`, `from`, `to`, `state`, `custom_headers` ...)"),
    responses(
        (status = 200, description = "Taken"),
        (status = 400, description = "Not a Call Control event", body = ErrorBody),
        (status = 401, description = "No valid Telnyx signature", body = ErrorBody),
        (status = 403, description = "The connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 503, description = "Telnyx signatures are not set up on the server (`TELNYX_PUBLIC_KEY`)", body = ErrorBody),
        (status = 500, description = "Database error (Telnyx sends it again)", body = ErrorBody),
    )
)]
pub async fn outbound(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let config = company_for(&state, &token).await?;
    verify(&state, &headers, &body, true)?;
    let envelope: Envelope = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("Not a Call Control event".into()))?;
    let data = envelope.data.unwrap_or_default();
    let payload = data.payload.unwrap_or_default();
    check_connection(
        &config,
        "outbound",
        text(payload.connection_id.as_ref()),
        &[config.credential_connection_id.as_deref()],
    )?;
    let pool = &state.pool;
    let kind = text(data.event_type.as_ref()).unwrap_or("unknown");
    let key = events::call_control_key(data.id.as_deref());
    let call = call_log::by_call_control(
        pool,
        config.company_id,
        payload.call_control_id(),
        payload.call_session_id(),
    )
    .await?;
    let raw = String::from_utf8_lossy(&body);
    let claimed = events::claim(
        pool,
        &Event {
            company_id: config.company_id,
            call_id: call.as_ref().map(|call| call.id),
            source: Source::CallControl,
            event_type: kind,
            dedupe_key: &key,
            occurred_at: events::parse_time(data.occurred_at.as_deref()),
            payload: &raw,
        },
    )
    .await?;
    if !claimed {
        return Ok(taken());
    }
    let occurred_at = events::parse_time(data.occurred_at.as_deref());
    let handled = match kind {
        "call.initiated" => initiated(&state, &config, &payload, &key).await,
        "call.answered" => answered(&state, &config, call.as_ref(), &payload, occurred_at).await,
        "call.hangup" => hung_up(&state, &config, call.as_ref(), &payload, occurred_at).await,
        "call.recording.saved" => recording_saved(&state, &config, call.as_ref(), &payload).await,
        _ => Ok(()),
    };
    if let Err(err) = handled {
        events::release(pool, &key).await?;
        return Err(err);
    }
    Ok(taken())
}

/// A phone of the company, as a parked call names it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
struct Device {
    id: i64,
    user_id: i32,
    /// Its device session (whose 911 alerts are deduplicated together, and
    /// whose call key signs its calls).
    session_id: i64,
    /// Its SIP user on Telnyx (`gencred...`).
    sip_username: String,
}

/// The SIP user (`gencred...`) of a `from` like `sip:gencred1@sip.telnyx.com`;
/// `None` when `from` is a phone number.
fn sip_user(from: Option<&str>) -> Option<&str> {
    let from = from?.trim();
    let from = from.strip_prefix("sip:").unwrap_or(from);
    let user = from.split('@').next()?.trim();
    user.starts_with("gencred").then_some(user)
}

/// The signed-in phone `id` of the company (with a credential).
async fn live_device(pool: &MySqlPool, company_id: i32, id: i64) -> sqlx::Result<Option<Device>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(pd.id AS SIGNED) AS id, pd.user_id, CAST(ds.id AS SIGNED) AS session_id,
                pd.sip_username
           FROM phone_devices pd
           JOIN device_sessions ds ON ds.id = pd.device_session_id
          WHERE {LIVE_DEVICE} AND pd.company_id = ? AND ds.company_id = pd.company_id
            AND pd.sip_username IS NOT NULL AND pd.id = ?"
    )))
    .bind(company_id)
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// What an app call's `X-Granite-Call-Auth` must sign.
struct Signed<'a> {
    /// `X-Granite-Device-Id`, as sent.
    device_id: &'a str,
    /// Where the call goes: E.164 (an N11 code as dialed).
    destination: &'a str,
    /// `X-Granite-Caller-Id`, as sent (empty when not sent).
    caller_id: &'a str,
}

/// Whether `header` (`v1.<unix s>.<base64url HMAC-SHA256>`) proves the call
/// comes from the phone whose call key is `key` ([`call_key`]): the
/// HMAC-SHA256, keyed with the key's 32 bytes, of
/// `v1|{device id}|{destination}|{caller id}|{unix s}`, compared in constant
/// time, its time at most [`CALL_AUTH_WINDOW_SECS`] from `now`. `Err` says
/// why not (no numbers in it).
fn check_call_auth(
    header: Option<&str>,
    key: &[u8; 32],
    signed: &Signed<'_>,
    now: i64,
) -> Result<(), &'static str> {
    let header = header.ok_or("no call signature")?;
    let mut parts = header.split('.');
    let (Some("v1"), Some(time), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("a malformed call signature");
    };
    let time: i64 = time.parse().map_err(|_| "a malformed call signature")?;
    if now.abs_diff(time) > CALL_AUTH_WINDOW_SECS.unsigned_abs() {
        return Err("a stale call signature");
    }
    let signature = URL_SAFE_NO_PAD
        .decode(signature.trim_end_matches('='))
        .map_err(|_| "a malformed call signature")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| "an unusable call key")?;
    mac.update(
        format!(
            "v1|{}|{}|{}|{time}",
            signed.device_id, signed.destination, signed.caller_id
        )
        .as_bytes(),
    );
    mac.verify_slice(&signature)
        .map_err(|_| "a call signature that does not match")
}

/// A parked call whose phone did not prove itself: why, and the phone it
/// named (`X-Granite-Device-Id`, when that is a number), for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Unproven {
    reason: &'static str,
    claimed: Option<i64>,
}

/// The phone that placed a parked call (see the module docs): the one
/// `X-Granite-Device-Id` names, proven by `X-Granite-Call-Auth` over the
/// call to `destination` with its call key (derived from `secret`, the
/// `SESSION_SECRET`); a SIP user in `from` must be that phone's.
async fn calling_device(
    pool: &MySqlPool,
    secret: &str,
    company_id: i32,
    payload: &Payload,
    destination: &str,
) -> sqlx::Result<Result<Device, Unproven>> {
    let Some(raw) = payload.header("x-granite-device-id") else {
        return Ok(Err(Unproven {
            reason: "no phone named",
            claimed: None,
        }));
    };
    let claimed = raw.parse::<i64>().ok();
    let refuse = |reason| Ok(Err(Unproven { reason, claimed }));
    let device = match claimed {
        Some(id) => live_device(pool, company_id, id).await?,
        None => None,
    };
    let Some(device) = device else {
        return refuse("not a signed-in phone of the company");
    };
    let signed = Signed {
        device_id: raw,
        destination,
        caller_id: payload.header("x-granite-caller-id").unwrap_or_default(),
    };
    let auth = check_call_auth(
        payload.header("x-granite-call-auth"),
        &call_key(secret, device.session_id),
        &signed,
        Utc::now().timestamp(),
    );
    if let Err(reason) = auth {
        return refuse(reason);
    }
    if sip_user(text(payload.from.as_ref())).is_some_and(|user| user != device.sip_username) {
        return refuse("the SIP user is not the phone's");
    }
    Ok(Ok(device))
}

/// The user, while they may still use Granite Phone in the company.
async fn phone_user(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<Option<SessionUser>> {
    let Some(user) = find_user(pool, user_id, company_id).await? else {
        return Ok(None);
    };
    let positions = user_positions(pool, user.id).await?;
    let user = effective_user(user, &positions, None);
    Ok((user.is_employee || user.is_admin || user.is_superuser).then_some(user))
}

/// An approved call.
struct Plan {
    user: Option<SessionUser>,
    /// Where the call goes: E.164, or the emergency code.
    to: String,
    caller: Option<CallerId>,
    customer_id: Option<i32>,
    deal_id: Option<i64>,
    ai_notes: bool,
    /// An emergency call: never refused, no limits, the admins alerted.
    emergency: Option<Emergency>,
}

/// An emergency call's alert.
struct Emergency {
    /// Whose alerts it is deduplicated with: the proven phone's session, else
    /// the company's.
    source: AlertSource,
}

/// A refused call.
struct Refusal {
    reason: &'static str,
    user_id: Option<i32>,
    other_number: String,
}

/// The user's caller ID for this call: the header's when it is one of
/// theirs, else (no header) their default.
fn pick_caller(
    caller_ids: &[CallerId],
    header: Option<&str>,
    default: Option<i32>,
) -> Option<CallerId> {
    header
        .map_or_else(
            || default_caller_id(caller_ids, default),
            |raw| {
                to_e164(raw).and_then(|e164| caller_ids.iter().find(|caller| caller.e164 == e164))
            },
        )
        .cloned()
}

/// An emergency call, never refused: from the user's caller ID when the
/// phone proved itself and the user has one, else a company number.
async fn emergency_plan(
    pool: &MySqlPool,
    company_id: i32,
    user: Option<SessionUser>,
    header_caller: Option<&str>,
    code: &str,
    source: AlertSource,
) -> sqlx::Result<Plan> {
    let mine = match &user {
        Some(user) => {
            let stored = settings::load(pool, user.id, company_id).await?;
            let caller_ids = lines::caller_ids(pool, company_id, user.id).await?;
            pick_caller(&caller_ids, header_caller, stored.default_caller_number_id).or_else(|| {
                default_caller_id(&caller_ids, stored.default_caller_number_id).cloned()
            })
        }
        None => None,
    };
    let caller = match mine {
        Some(caller) => Some(caller),
        None => lines::company_number(pool, company_id).await?,
    };
    Ok(Plan {
        user,
        to: code.to_string(),
        caller,
        customer_id: None,
        deal_id: None,
        ai_notes: false,
        emergency: Some(Emergency { source }),
    })
}

/// Whether a parked call may go ahead (reads only: its call slot is taken
/// when it is logged, in [`approve`]).
async fn decide(
    state: &AppState,
    config: &CompanyConfig,
    payload: &Payload,
) -> sqlx::Result<Result<Plan, Refusal>> {
    let pool = &state.pool;
    let company_id = config.company_id;
    let dialed = text(payload.to.as_ref()).unwrap_or_default();
    let dialed = dialed.strip_prefix("sip:").unwrap_or(dialed);
    let dialed = dialed.split('@').next().unwrap_or_default().trim();
    let emergency = emergency_code(dialed);
    let destination = emergency.map_or_else(|| to_e164(dialed).unwrap_or_default(), str::to_string);
    let device = calling_device(
        pool,
        &state.session_secret,
        company_id,
        payload,
        &destination,
    )
    .await?;
    let user = match &device {
        Ok(device) => phone_user(pool, device.user_id, company_id).await?,
        Err(_) => None,
    };
    let header_caller = payload.header("x-granite-caller-id");
    if let Some(code) = emergency {
        let (source, claimed) = match &device {
            Ok(device) => (AlertSource::Session(device.session_id), Some(device.id)),
            Err(unproven) => (AlertSource::Company, unproven.claimed),
        };
        tracing::warn!(
            company_id,
            code,
            claimed_device = claimed,
            proven = device.is_ok(),
            "emergency call from the app; connecting it"
        );
        return Ok(Ok(emergency_plan(
            pool,
            company_id,
            user,
            header_caller,
            code,
            source,
        )
        .await?));
    }
    let other_number = to_e164(dialed).unwrap_or_else(|| dialed.chars().take(20).collect());
    let refuse =
        |reason: &'static str, user_id: Option<i32>| -> sqlx::Result<Result<Plan, Refusal>> {
            Ok(Err(Refusal {
                reason,
                user_id,
                other_number: other_number.clone(),
            }))
        };
    let user_id = user.as_ref().map(|user| user.id);
    if !config.calling_on() {
        return refuse("calling is switched off for the company", user_id);
    }
    let Some(to) = to_e164(dialed).filter(|to| is_nanp(to)) else {
        return refuse("not a callable US or Canada number", user_id);
    };
    if let Err(unproven) = device {
        tracing::warn!(
            company_id,
            claimed_device = unproven.claimed,
            "app call of an unproven phone"
        );
        return refuse(unproven.reason, None);
    }
    let Some(user) = user else {
        return refuse("the phone's user may not use Granite Phone", None);
    };
    let stored = settings::load(pool, user.id, company_id).await?;
    let caller_ids = lines::caller_ids(pool, company_id, user.id).await?;
    let Some(caller) = pick_caller(&caller_ids, header_caller, stored.default_caller_number_id)
    else {
        return refuse("not one of the user's caller IDs", Some(user.id));
    };
    let sent_customer = payload
        .header("x-granite-customer-id")
        .and_then(|raw| raw.parse::<i32>().ok());
    let customer_id = match sent_customer {
        Some(id) => {
            sqlx::query_scalar(
                "SELECT id FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL AND id <> ?",
            )
            .bind(id)
            .bind(company_id)
            .bind(SAMPLE_CUSTOMER_ID)
            .fetch_optional(pool)
            .await?
        }
        None => None,
    };
    let customer_id = match customer_id {
        Some(id) => Some(id),
        None => match last10(&to) {
            Some(digits) => contacts::customer_for_number(pool, company_id, &digits).await?,
            None => None,
        },
    };
    let deal_id = call_log::open_deal(pool, customer_id).await?;
    Ok(Ok(Plan {
        user: Some(user),
        to,
        caller: Some(caller),
        customer_id,
        deal_id,
        ai_notes: stored.ai_notes,
        emergency: None,
    }))
}

/// `call.initiated`: approve or refuse the app's parked call.
async fn initiated(
    state: &AppState,
    config: &CompanyConfig,
    payload: &Payload,
    key: &str,
) -> ApiResult<()> {
    let parked = text(payload.state.as_ref()) == Some("parked")
        && text(payload.direction.as_ref()) != Some("incoming");
    let Some(call_control_id) = payload.call_control_id().filter(|_| parked) else {
        return Ok(());
    };
    match decide(state, config, payload).await? {
        Ok(plan) => approve(state, config, payload, call_control_id, &plan, key).await,
        Err(refusal) => {
            refuse(state, config, payload, call_control_id, &refusal, key).await;
            Ok(())
        }
    }
}

/// Logs the approved call, then transfers it.
///
/// Only the log can fail the event: once Telnyx was asked, failures are
/// logged. A call that is no emergency is logged in the transaction that
/// takes its call slot: over the user's limits it is refused instead.
async fn approve(
    state: &AppState,
    config: &CompanyConfig,
    payload: &Payload,
    call_control_id: &str,
    plan: &Plan,
    key: &str,
) -> ApiResult<()> {
    let pool = &state.pool;
    let call = call_log::NewOutbound {
        company_id: config.company_id,
        user_id: plan.user.as_ref().map(|user| user.id),
        status: CallStatus::Ringing,
        other_number: &plan.to,
        customer_id: plan.customer_id,
        deal_id: plan.deal_id,
        caller: plan.caller.as_ref(),
        call_control_id: Some(call_control_id),
        call_session_id: payload.call_session_id(),
        ai_notes: plan.ai_notes,
    };
    let id = match (&plan.emergency, &plan.user) {
        (None, Some(user)) => {
            let tz = company::time_zone_of(Some(config));
            let now = Utc::now();
            let daily = Daily {
                limit: config.daily_call_limit,
                since: clock::day_start(tz, now),
                until: clock::next_day_start(tz, now),
            };
            let mut tx = pool.begin().await?;
            if let Err(wait) =
                limits::take_call_slot(&mut tx, config.company_id, user.id, Some(&daily)).await?
            {
                tx.rollback().await?;
                let refusal = Refusal {
                    reason: wait.reason(),
                    user_id: Some(user.id),
                    other_number: plan.to.clone(),
                };
                refuse(state, config, payload, call_control_id, &refusal, key).await;
                return Ok(());
            }
            let id = call_log::insert_outbound(&mut *tx, &call).await?;
            tx.commit().await?;
            id
        }
        _ => call_log::insert_outbound(pool, &call).await?,
    };
    if let Err(err) = events::link_call(pool, key, id).await {
        tracing::warn!(call_id = id, error = %err, "event not linked to its call");
    }
    let record = plan.emergency.is_none() && records_outbound(config);
    // A transfer plays only an audio file to the customer before bridging.
    // Without the company's notice recording the call is bridged unrecorded,
    // and `answered` says the notice to both sides, then starts recording.
    let notice_url = config.recording_notice_url.clone().filter(|_| record);
    let record_from_answer = notice_url.is_some();
    let transfer = Transfer {
        to: plan.to.clone(),
        from: plan.caller.as_ref().map(|caller| caller.e164.clone()),
        audio_url: notice_url,
        record: record_from_answer.then(|| "record-from-answer".to_string()),
        record_channels: record_from_answer.then(|| "dual".to_string()),
        record_format: record_from_answer.then(|| "mp3".to_string()),
        timeout_secs: Some(RING_SECS),
        webhook_url: Some(format!("{}/outbound", config.webhook_base(&state.phone))),
        command_id: Some(format!("approve-{id}")),
        client_state: None,
        target_leg_client_state: (record && !record_from_answer)
            .then(|| SPOKEN_NOTICE_STATE.to_string()),
    };
    match state.telnyx.transfer(call_control_id, &transfer).await {
        Ok(()) => {
            let approved = sqlx::query("UPDATE phone_calls SET approved = 1 WHERE id = ?")
                .bind(id)
                .execute(pool)
                .await;
            if let Err(err) = approved {
                tracing::warn!(call_id = id, error = %err, "approval not saved");
            }
        }
        Err(err) => {
            tracing::warn!(call_id = id, error = %err, "approved call not transferred; hanging up");
            hang_up(state, call_control_id, &format!("hangup-{id}")).await;
            let failed = sqlx::query(
                "UPDATE phone_calls SET status = 'failed', ended_at = UTC_TIMESTAMP(3)
                  WHERE id = ? AND ended_at IS NULL",
            )
            .bind(id)
            .execute(pool)
            .await;
            if let Err(err) = failed {
                tracing::warn!(call_id = id, error = %err, "failed call not saved");
            }
        }
    }
    if let Some(emergency) = &plan.emergency {
        alert_admins(state, config, plan, emergency, id).await;
    }
    Ok(())
}

/// The 911 alert of an emergency call `call_id` (best effort; the user is
/// named only when the phone proved itself).
async fn alert_admins(
    state: &AppState,
    config: &CompanyConfig,
    plan: &Plan,
    emergency: &Emergency,
    call_id: i64,
) {
    let who = plan.user.as_ref().map_or_else(
        || "A Granite Phone user".to_string(),
        |user| {
            user.name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(&user.email)
                .to_string()
        },
    );
    let caller = plan.user.as_ref().map(|user| user.id);
    match alerts::alert_e911(
        state,
        config.company_id,
        emergency.source,
        caller,
        &who,
        &plan.to,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => tracing::info!(call_id, "911 alert sent in the last 10 minutes already"),
        Err(err) => tracing::warn!(call_id, error = %err, "911 alert not sent"),
    }
}

/// Hangs a refused call up and logs it `failed` (fail closed: the hangup
/// goes out even when the log cannot be written).
async fn refuse(
    state: &AppState,
    config: &CompanyConfig,
    payload: &Payload,
    call_control_id: &str,
    refusal: &Refusal,
    key: &str,
) {
    tracing::warn!(
        company_id = config.company_id,
        reason = refusal.reason,
        "app call refused"
    );
    let logged = call_log::insert_outbound(
        &state.pool,
        &call_log::NewOutbound {
            company_id: config.company_id,
            user_id: refusal.user_id,
            status: CallStatus::Failed,
            other_number: &refusal.other_number,
            customer_id: None,
            deal_id: None,
            caller: None,
            call_control_id: Some(call_control_id),
            call_session_id: payload.call_session_id(),
            ai_notes: false,
        },
    )
    .await;
    let command = match &logged {
        Ok(id) => format!("hangup-{id}"),
        Err(err) => {
            tracing::warn!(error = %err, "refused call not logged");
            format!("hangup-{}", &key[..16.min(key.len())])
        }
    };
    if let Ok(id) = logged
        && let Err(err) = events::link_call(&state.pool, key, id).await
    {
        tracing::warn!(call_id = id, error = %err, "event not linked to its call");
    }
    hang_up(state, call_control_id, &command).await;
}

/// Hangs a call up; a call that ended already is fine.
async fn hang_up(state: &AppState, call_control_id: &str, command_id: &str) {
    if let Err(err) = state.telnyx.hangup(call_control_id, command_id).await
        && err.code() != Some("90018")
    {
        tracing::warn!(error = %err, "call not hung up");
    }
}

/// `call.answered` of the customer's leg: the call is answered, at Telnyx's
/// time of the answer, and a call approved for recording after the spoken
/// notice gets it ([`record_after_notice`]). The app's own leg answering is
/// not the customer; `call.bridged` is not handled at all (a transfer
/// bridges before the customer's phone stops ringing).
async fn answered(
    state: &AppState,
    config: &CompanyConfig,
    call: Option<&CallFacts>,
    payload: &Payload,
    occurred_at: Option<DateTime<Utc>>,
) -> ApiResult<()> {
    let Some(call) = call else {
        return Ok(());
    };
    let own_leg = payload.call_control_id().is_some()
        && payload.call_control_id() == call.telnyx_call_control_id.as_deref();
    if own_leg {
        return Ok(());
    }
    call_log::answered(
        &state.pool,
        config.company_id,
        call.id,
        Answer {
            at: occurred_at,
            ..Answer::default()
        },
    )
    .await?;
    if text(payload.client_state.as_ref()) == Some(SPOKEN_NOTICE_STATE) {
        record_after_notice(state, call).await;
    }
    Ok(())
}

/// Says the recording notice to both sides of the answered call, then
/// records it (both sides, from now), on the app's leg. Best effort: when
/// the notice cannot be said, nothing is recorded (logged).
async fn record_after_notice(state: &AppState, call: &CallFacts) {
    let Some(leg) = call.telnyx_call_control_id.as_deref() else {
        return;
    };
    let started = match state
        .telnyx
        .speak(leg, NOTICE, &format!("notice-{}", call.id))
        .await
    {
        Ok(()) => {
            state
                .telnyx
                .record_start(leg, &format!("record-{}", call.id))
                .await
        }
        Err(err) => Err(err),
    };
    if let Err(err) = started {
        tracing::warn!(call_id = call.id, error = %err, "call not recorded: the notice or the recording did not start");
    }
}

/// What an app call nobody answered becomes, by Telnyx's hangup cause.
fn unanswered_status(cause: Option<&str>, source: Option<&str>) -> CallStatus {
    match cause.unwrap_or_default() {
        "user_busy" | "call_rejected" => CallStatus::Busy,
        "normal_clearing" if source == Some("callee") => CallStatus::NoAnswer,
        "no_answer" | "timeout" => CallStatus::NoAnswer,
        "originator_cancel" | "normal_clearing" => CallStatus::Canceled,
        _ => CallStatus::Failed,
    }
}

/// `call.hangup`: the first leg to hang up ends the call (at Telnyx's time
/// of the hangup).
async fn hung_up(
    state: &AppState,
    config: &CompanyConfig,
    call: Option<&CallFacts>,
    payload: &Payload,
    occurred_at: Option<DateTime<Utc>>,
) -> ApiResult<()> {
    let Some(call) = call else {
        return Ok(());
    };
    let cause = text(payload.hangup_cause.as_ref());
    let unanswered = unanswered_status(cause, text(payload.hangup_source.as_ref()));
    if let Some(ended) = call_log::finalize(
        &state.pool,
        config.company_id,
        call.id,
        unanswered,
        cause,
        End::at(occurred_at),
    )
    .await?
    {
        outcomes::after_end(state, &ended).await;
    }
    Ok(())
}

/// `call.recording.saved`: copy the recording.
async fn recording_saved(
    state: &AppState,
    config: &CompanyConfig,
    call: Option<&CallFacts>,
    payload: &Payload,
) -> ApiResult<()> {
    let (Some(call), Some(recording_id)) = (call, text(payload.recording_id.as_ref())) else {
        return Ok(());
    };
    recordings::copy(
        state,
        config.company_id,
        call.id,
        recording_id,
        RecordingKind::Call,
        None,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sip_user_is_read_from_from() {
        assert_eq!(
            sip_user(Some("sip:gencred1@sip.telnyx.com")),
            Some("gencred1")
        );
        assert_eq!(
            sip_user(Some(&format!("gencred{}", "x".repeat(200)))).map(str::len),
            Some(207)
        );
        assert_eq!(
            sip_user(Some(" gencredAbC@sip.telnyx.com ")),
            Some("gencredAbC")
        );
        assert_eq!(sip_user(Some("gencred9")), Some("gencred9"));
        // A phone number (the caller ID the app set) names no phone.
        assert_eq!(sip_user(Some("+13175550151")), None);
        assert_eq!(sip_user(Some("sip:+13175550151@sip.telnyx.com")), None);
        assert_eq!(sip_user(None), None);
    }

    #[test]
    fn unanswered_app_calls_end_by_their_hangup_cause() {
        let cases = [
            (
                Some("originator_cancel"),
                Some("caller"),
                CallStatus::Canceled,
            ),
            (
                Some("normal_clearing"),
                Some("caller"),
                CallStatus::Canceled,
            ),
            (
                Some("normal_clearing"),
                Some("callee"),
                CallStatus::NoAnswer,
            ),
            (Some("user_busy"), Some("callee"), CallStatus::Busy),
            (Some("call_rejected"), None, CallStatus::Busy),
            (Some("timeout"), None, CallStatus::NoAnswer),
            (Some("not_found"), None, CallStatus::Failed),
            (None, None, CallStatus::Failed),
        ];
        for (cause, source, status) in cases {
            assert_eq!(
                unanswered_status(cause, source),
                status,
                "{cause:?} {source:?}"
            );
        }
    }

    /// The app's `X-Granite-Call-Auth` (`CallEngine.callAuth`), made apart
    /// from the server's check: the key is the 32 bytes of the base64url
    /// `callKey` the phone got at sign-in.
    fn app_signature(call_key: &str, message: &str, time: i64) -> String {
        let key = URL_SAFE_NO_PAD.decode(call_key).unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(&key).unwrap();
        mac.update(message.as_bytes());
        format!(
            "v1.{time}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        )
    }

    #[test]
    fn call_signatures_are_keyed_with_the_phones_call_key() {
        // Session 950041's key under the secret "test-secret", as sign-in
        // hands it out.
        let key = call_key("test-secret", 950_041);
        let given = crate::auth::device::encoded_call_key("test-secret", 950_041);
        let signed = Signed {
            device_id: "960041",
            destination: "+13175550142",
            caller_id: "+13175550151",
        };
        let now = 1_792_000_000;
        let good = app_signature(
            &given,
            "v1|960041|+13175550142|+13175550151|1792000000",
            now,
        );
        // Known answer (Node's crypto, apart from both).
        assert_eq!(
            good,
            "v1.1792000000.MBspGnZnuPoXcUKywqWR14eg8afD86oWqnWGrRsWtaE"
        );
        assert_eq!(check_call_auth(Some(&good), &key, &signed, now), Ok(()));
        // Ten minutes either way is fine; more is a replay or a broken clock.
        assert_eq!(
            check_call_auth(Some(&good), &key, &signed, now + 600),
            Ok(())
        );
        assert_eq!(
            check_call_auth(Some(&good), &key, &signed, now - 600),
            Ok(())
        );
        assert_eq!(
            check_call_auth(Some(&good), &key, &signed, now + 601),
            Err("a stale call signature")
        );
        // Another phone's key (another session, or another server's secret).
        for other in [
            call_key("test-secret", 950_043),
            call_key("another-secret", 950_041),
        ] {
            assert_eq!(
                check_call_auth(Some(&good), &other, &signed, now),
                Err("a call signature that does not match")
            );
        }
        // Another destination, caller ID or phone than was signed.
        for changed in [
            Signed {
                destination: "+13175550143",
                ..signed
            },
            Signed {
                caller_id: "+13175550154",
                ..signed
            },
            Signed {
                device_id: "960043",
                ..signed
            },
        ] {
            assert_eq!(
                check_call_auth(Some(&good), &key, &changed, now),
                Err("a call signature that does not match")
            );
        }
        // Padding is tolerated; anything else malformed is refused.
        assert_eq!(
            check_call_auth(Some(&format!("{good}=")), &key, &signed, now),
            Ok(())
        );
        let body = good.rsplit('.').next().unwrap();
        for bad in [
            format!("v2.{now}.{body}"),
            format!("v1.soon.{body}"),
            format!("v1.{now}"),
            format!("v1.{now}.{body}.x"),
            format!("v1.{now}.not base64!"),
        ] {
            assert_eq!(
                check_call_auth(Some(&bad), &key, &signed, now),
                Err("a malformed call signature"),
                "{bad}"
            );
        }
        assert_eq!(
            check_call_auth(None, &key, &signed, now),
            Err("no call signature")
        );
    }

    #[test]
    fn custom_headers_are_matched_in_any_case() {
        let payload: Payload = serde_json::from_value(serde_json::json!({
            "custom_headers": [
                { "name": "X-Granite-Device-Id", "value": " 42 " },
                { "header_name": "x-granite-caller-id", "header_value": "+13175550151" },
                { "name": "X-Empty", "value": "" }
            ]
        }))
        .unwrap();
        assert_eq!(payload.header("x-granite-device-id"), Some("42"));
        assert_eq!(payload.header("X-GRANITE-CALLER-ID"), Some("+13175550151"));
        assert_eq!(payload.header("x-empty"), None);
        assert_eq!(payload.header("x-granite-customer-id"), None);
        let none: Payload =
            serde_json::from_value(serde_json::json!({ "custom_headers": null })).unwrap();
        assert_eq!(none.header("x-granite-device-id"), None);
    }
}
