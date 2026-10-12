//! SMS conversations through `CloudTalk` or `RingCentral` (the `{provider}`
//! path segment).
//!
//! - `GET  /v1/sms/{provider}/threads`                → a page of the inbox
//! - `GET  /v1/sms/{provider}/threads/{phone}`        → one conversation
//! - `POST /v1/sms/{provider}/threads/{phone}/read`   → mark it read
//! - `POST /v1/sms/{provider}/threads/{phone}/unread` → mark it unread
//! - `GET  /v1/sms/{provider}/unread-count`           → the sidebar badge
//! - `GET  /v1/sms/{provider}/desktop-alerts`         → unread texts to notify about
//! - `GET  /v1/sms/{provider}/sales-reps`             → the admin inbox's rep filter
//! - `POST /v1/sms/{provider}/messages`               → send a text (or schedule it)
//! - `GET  /v1/sms/{provider}/history`                → a deal's or customer's texts
//! - `GET|POST /v1/sms-flows`, `GET|PUT|DELETE /v1/sms-flows/{id}` → SMS flows
//!
//! Replaces the Remix `api.cloudtalk.sms.*` / `api.ringcentral.sms.*` read
//! routes (`app/utils/smsProviderService.server.ts`). Reads use the columns
//! the writers derive on insert (`common::sms::derived`); the upkeep that
//! fails stuck sends runs here, at most once a minute per company.

mod flows;
mod history;
pub mod provider;
mod rollup;
mod schemas;
mod scope;
pub mod send;
mod thread;
mod threads;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(threads::list_threads))
        .routes(routes!(threads::unread_count))
        .routes(routes!(threads::desktop_alerts))
        .routes(routes!(threads::sales_reps))
        .routes(routes!(thread::get_thread))
        .routes(routes!(thread::mark_read))
        .routes(routes!(thread::mark_unread))
        .routes(routes!(send::send))
        .routes(routes!(history::history))
        .routes(routes!(flows::list, flows::create))
        .routes(routes!(flows::get, flows::update, flows::delete))
}
