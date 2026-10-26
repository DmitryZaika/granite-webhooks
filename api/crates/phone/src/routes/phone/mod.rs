//! Granite Phone: the app's routes and the web admin pages.
//!
//! The app's own routes (`/v1/phone/...`) take the device token via
//! `auth::device::DeviceUser` or `auth::device::PhoneCaller`; the web admin
//! pages (`/v1/phone/admin/...` and `/v1/superuser/companies/{id}/telnyx`)
//! take the session cookie only.
//!
//! - `GET  /v1/phone/bootstrap`                → everything the app needs at start
//! - `POST /v1/phone/devices`                  → register this phone (Telnyx credential)
//! - `POST /v1/phone/devices/{id}/token`       → a Telnyx login JWT (1 a minute, 6 an hour)
//! - `PUT  /v1/phone/devices/{id}/push-token`  → the phone's FCM token
//! - `GET  /v1/phone/caller-ids`               → numbers I may call from
//! - `PUT  /v1/phone/settings`                 → change my phone settings
//! - `PUT  /v1/phone/availability`             → Available / Do not disturb
//! - `POST /v1/phone/notices/accept`           → accept the recording or 911 notice
//! - `GET  /v1/phone/lines`                   → my lines and what their unanswered calls do
//! - `GET|POST /v1/phone/call-rules`, `PUT|DELETE /v1/phone/call-rules/{id}` → my incoming call rules
//! - `GET|PUT /v1/phone/call-blocking`         → the company's spam and blocked numbers (admins)
//! - `GET  /v1/phone/hold-music`, `POST /v1/phone/hold-music/upload` → hold music tracks, my upload
//! - `PUT  /v1/phone/calls/hold`               → I held or resumed my live call: hold music on or off
//! - `POST /v1/phone/cell/verify`, `/confirm`  → verify my cell with a code call
//! - `GET  /v1/phone/contacts/index`           → the offline contact index (paged, since, ETag)
//! - `POST /v1/phone/contacts/lookup`          → who these numbers are
//! - `GET  /v1/phone/contacts/{id}`            → a contact card
//! - `POST /v1/phone/contacts`                 → a new customer (me as rep, an open deal)
//! - `POST /v1/phone/contacts/{id}/phones`     → add a number to a customer
//! - `GET  /v1/phone/calls`, `/calls/{id}`     → Recents, one call
//! - `POST /v1/phone/calls/seen`, `DELETE /v1/phone/calls/{id}` → mark seen, hide
//! - `GET  /v1/phone/calls/stats`              → my talk time today, this week, this month
//! - `GET  /v1/phone/calls/{id}/recording`     → a 5-minute link to the recording
//! - `POST /v1/phone/calls/{id}/note`          → a deal note about a call
//! - `POST /v1/phone/calls/via-cell`           → Call via my cell
//! - `POST /v1/phone/calls/test`               → our server calls this phone
//! - `POST /v1/phone/calls/decline`            → declined on this phone: stop ringing
//! - `POST /v1/phone/e911`                     → 911 was dialed: alert the admins
//! - `POST /v1/phone/diagnostics`              → "Report a problem"
//! - `GET  /v1/phone/texts/threads`            → the Texts tab: my threads (line + other number)
//! - `GET  /v1/phone/texts/threads/{key}/messages`, `POST .../read` → a thread's texts, mark it read
//! - `GET  /v1/phone/texts/messages/{id}/thread` → the thread of a text (the `text` push's `textId`)
//! - `GET  /v1/phone/texts/media/{id}`         → a 5-minute link to a text's picture or file
//! - `POST /v1/phone/texts`                    → text from one of my lines
//! - `POST /v1/phone/pushes/test`              → send me a test push (temporary)
//!
//! The other Lambdas (job key), for what they also send to Telegram:
//!
//! - `POST /v1/phone/pushes/notification`      → push a CRM notification to its user's phones
//! - `POST /v1/phone/pushes/lead`              → push a lead alert to the rep's or managers' phones
//!
//! Admin pages (session cookie):
//!
//! - `GET    /v1/phone/admin/numbers`          → the company's numbers, users, calling status
//! - `PUT    /v1/phone/admin/numbers/{id}`     → change a number, who it rings (audited)
//! - `POST   /v1/phone/admin/numbers/import`   → add the TeXML app's numbers from Telnyx
//! - `GET    /v1/phone/admin/devices`          → the company's signed-in phones
//! - `DELETE /v1/phone/admin/devices/{id}`     → sign a company phone out (audited)
//! - `GET|PUT /v1/superuser/companies/{id}/telnyx` → a company's Telnyx setup (superusers)
//! - `/v1/phone/admin/ports*`            → moving existing numbers to Telnyx (`porting.rs`)
//! - `/v1/phone/admin/tendlc*`, `/v1/superuser/tendlc`, `/v1/superuser/companies/{id}/tendlc*`
//!   → the 10DLC texting registration: Managers fill it in, superusers send it (`tendlc.rs`)
//!
//! The CRM's phone dock (session cookie; the browser is a phone too):
//!
//! - `GET  /v1/phone/web/bootstrap`              → calling on, caller IDs, settings, recording mode
//! - `POST /v1/phone/web/devices`                → register this browser (credential, call key)
//! - `POST /v1/phone/web/devices/{id}/token`     → a Telnyx login JWT (1 a minute, 6 an hour)
//! - `PUT  /v1/phone/web/devices/{id}/presence`  → connected: ring this browser (or stop)
//! - `POST /v1/phone/web/devices/{id}/decline`   → declined in the browser: stop ringing
//!
//! `POST /v1/phone/calls/via-cell` and `/v1/phone/cell/verify|confirm`
//! take the cookie too, and so does `PUT /v1/phone/calls/recording` (the
//! Record button of a live call: start, pause, resume).
//!
//! Shared logic lives in `crate::phone`; the contract is
//! `docs/api-contract.md` of the granite-phone repo.

mod admin;
mod blocking;
mod bootstrap;
mod call_rules;
mod calls;
mod cell;
mod connect;
mod contacts;
mod devices;
mod hold;
mod porting;
mod pushes;
mod recording;
mod settings;
mod superuser;
mod support;
mod tendlc;
mod test_pushes;
mod texts;
mod web;

use crate::error::ApiError;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::phone::company::{self, CompanyConfig};
use crate::state::AppState;
use crate::telnyx::TelnyxError;
use axum::Json;
use axum::http::StatusCode;
use axum::http::header::RETRY_AFTER;
use axum::response::{IntoResponse, Response};
use lambda_http::tracing;
use serde::Serialize;
use sqlx::MySqlExecutor;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(bootstrap::bootstrap))
        .routes(routes!(devices::register_device))
        .routes(routes!(devices::mint_device_token))
        .routes(routes!(devices::set_device_push_token))
        .routes(routes!(settings::list_caller_ids))
        .routes(routes!(settings::update_settings))
        .routes(routes!(settings::set_availability))
        .routes(routes!(settings::accept_notice))
        .routes(routes!(cell::verify_cell))
        .routes(routes!(cell::confirm_cell))
        .routes(routes!(contacts::contact_index))
        .routes(routes!(contacts::lookup_contacts))
        .routes(routes!(contacts::create_contact))
        .routes(routes!(contacts::contact_card))
        .routes(routes!(contacts::add_contact_phone))
        .routes(routes!(calls::list_calls))
        .routes(routes!(calls::get_call, calls::hide_call))
        .routes(routes!(calls::mark_calls_seen))
        .routes(routes!(calls::call_stats))
        .routes(routes!(calls::call_recording))
        .routes(routes!(calls::add_call_note))
        .routes(routes!(calls::call_via_cell))
        .routes(routes!(calls::test_call))
        .routes(routes!(calls::decline_call))
        .routes(routes!(support::report_e911))
        .routes(routes!(support::send_diagnostics))
        .routes(routes!(texts::list_text_threads))
        .routes(routes!(texts::list_text_messages))
        .routes(routes!(texts::text_thread))
        .routes(routes!(texts::text_media))
        .routes(routes!(texts::send_text))
        .routes(routes!(texts::mark_text_thread_read))
        .routes(routes!(admin::list_admin_numbers))
        .routes(routes!(admin::update_admin_number))
        .routes(routes!(admin::import_admin_numbers))
        .routes(routes!(admin::list_admin_devices))
        .routes(routes!(admin::revoke_admin_device))
        .routes(routes!(admin::get_admin_hours, admin::save_admin_hours))
        .routes(routes!(
            admin::get_admin_recording,
            admin::save_admin_recording
        ))
        .routes(routes!(connect::connect_telnyx))
        .routes(routes!(connect::search_numbers))
        .routes(routes!(connect::buy_number))
        .routes(routes!(connect::connect_company_telnyx))
        .routes(routes!(
            superuser::company_telnyx_config,
            superuser::save_company_telnyx_config
        ))
        .merge(porting::router())
        .routes(routes!(pushes::push_notification_route))
        .routes(routes!(pushes::push_lead_route))
        .routes(routes!(test_pushes::send_test_push))
        .routes(routes!(web::web_bootstrap))
        .routes(routes!(web::register_web_phone))
        .routes(routes!(web::mint_web_phone_token))
        .routes(routes!(web::set_web_phone_presence))
        .routes(routes!(web::decline_web_phone_call))
        .routes(routes!(recording::set_live_call_recording))
        .routes(routes!(hold::set_live_call_hold))
        .routes(routes!(hold::list_hold_music))
        .routes(routes!(hold::start_hold_music_upload))
        .routes(routes!(call_rules::list_lines))
        .routes(routes!(
            call_rules::list_call_rules,
            call_rules::create_call_rule
        ))
        .routes(routes!(
            call_rules::update_call_rule,
            call_rules::delete_call_rule
        ))
        .routes(routes!(
            blocking::get_call_blocking,
            blocking::save_call_blocking
        ))
        .merge(tendlc::router())
}

/// Writes one entry of the phone audit log (`phone_audit_log`): `actor`
/// did `action` to `target` in the company, with `details` (JSON).
async fn audit<'e>(
    executor: impl MySqlExecutor<'e>,
    company_id: i32,
    actor: i32,
    action: &str,
    target: &str,
    details: &serde_json::Value,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO phone_audit_log (company_id, actor_user_id, action, target, details)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(company_id)
    .bind(actor)
    .bind(action)
    .bind(target.chars().take(128).collect::<String>())
    .bind(details.to_string())
    .execute(executor)
    .await?;
    Ok(())
}

/// `429`: wait before trying again.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RateLimited {
    /// `rate_limited`.
    pub error: String,
    /// Seconds to wait (also in the `Retry-After` header).
    pub retry_after_secs: u64,
}

/// `429 {"error": "rate_limited", "retryAfterSecs": n}` with `Retry-After`.
fn rate_limited(retry_after_secs: u64) -> Response {
    let retry_after_secs = retry_after_secs.max(1);
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(RETRY_AFTER, retry_after_secs.to_string())],
        Json(RateLimited {
            error: "rate_limited".into(),
            retry_after_secs,
        }),
    )
        .into_response()
}

/// `{"error": token}` errors the app reads.
fn denied(token: &str) -> ApiError {
    ApiError::Denied(token.into())
}

fn bad_request(token: &str) -> ApiError {
    ApiError::BadRequest(token.into())
}

/// The company's Telnyx setup when our server can place calls for it:
/// else 403 `calling_disabled`, or 503 when Telnyx is not set up on this
/// server.
async fn calling_setup(state: &AppState, company_id: i32) -> Result<CompanyConfig, ApiError> {
    let config = company::load(&state.pool, company_id)
        .await?
        .filter(CompanyConfig::can_place_calls)
        .ok_or_else(|| denied("calling_disabled"))?;
    if !state.telnyx.is_enabled() {
        return Err(ApiError::Unavailable(
            "Calling is not set up on the server.".into(),
        ));
    }
    Ok(config)
}

/// A failed Telnyx request, logged without its details: 503 when Telnyx is
/// not set up here, else 502.
fn telnyx_failed(err: &TelnyxError, what: &'static str) -> ApiError {
    tracing::warn!(error = %err, what, "Telnyx request failed");
    match err {
        TelnyxError::NotConfigured => {
            ApiError::Unavailable("Calling is not set up on the server.".into())
        }
        _ => ApiError::Upstream("Telnyx did not take the request. Try again.".into()),
    }
}
