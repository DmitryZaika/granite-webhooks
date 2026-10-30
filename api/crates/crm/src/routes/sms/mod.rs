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
//! - `GET  /v1/sms/all/threads[/{phone}]`, `GET /v1/sms/all/sales-reps`
//!   → the admin inbox over every provider, merged by phone
//! - `POST /v1/sms/{provider}/messages`               → send a text (or schedule it)
//! - `POST /v1/sms/attachments`                       → make uploaded files ready to send
//! - `POST /v1/sms/ringcentral/inbound-media`         → keep an inbound MMS's files (`webhooks` Lambda)
//! - `GET  /v1/sms/{provider}/history`                → a deal's or customer's texts
//! - `GET|POST /v1/sms-flows`, `GET|PUT|DELETE /v1/sms-flows/{id}` → SMS flows
//!
//! Replaces the Remix `api.cloudtalk.sms.*` / `api.ringcentral.sms.*` read
//! routes (`app/utils/smsProviderService.server.ts`). Reads use the columns
//! the writers derive on insert (`common::sms::derived`); the upkeep that
//! fails stuck sends runs here, at most once a minute per company.

mod all;
mod attachments;
pub mod flow_stops;
pub mod flows;
mod history;
mod inbound_media;
pub mod moves;
pub mod provider;
mod rollup;
pub mod schemas;
pub mod scope;
pub mod send;
pub mod thread;
mod threads;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(all::list_threads))
        .routes(routes!(all::get_thread))
        .routes(routes!(all::sales_reps))
        .routes(routes!(all::list_my_threads))
        .routes(routes!(all::get_my_thread))
        .routes(routes!(all::my_unread_count))
        .routes(routes!(all::my_desktop_alerts))
        .routes(routes!(threads::list_threads))
        .routes(routes!(threads::unread_count))
        .routes(routes!(threads::desktop_alerts))
        .routes(routes!(threads::sales_reps))
        .routes(routes!(thread::get_thread))
        .routes(routes!(thread::mark_read))
        .routes(routes!(thread::mark_unread))
        .routes(routes!(send::send))
        .routes(routes!(attachments::prepare_attachments))
        .routes(routes!(inbound_media::store_inbound_media))
        .routes(routes!(history::history))
        .routes(routes!(flows::list, flows::create))
        .routes(routes!(flows::get, flows::update, flows::delete))
}
