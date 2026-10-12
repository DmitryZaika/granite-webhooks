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
//!
//! Replaces the Remix `api.cloudtalk.sms.*` / `api.ringcentral.sms.*` read
//! routes (`app/utils/smsProviderService.server.ts`). Reads use the columns
//! the writers derive on insert (`common::sms::derived`); the upkeep that
//! fails stuck sends runs here, at most once a minute per company.

mod provider;
mod rollup;
mod schemas;
mod scope;
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
}
