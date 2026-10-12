//! Schedule events.
//!
//! - `POST /v1/events` → add one
//! - `PUT /v1/events/{id}` → edit (re-times its appointment reminders)
//! - `DELETE /v1/events/{id}` → delete (cancels its reminders)
//! - `POST /v1/events/estimates` → book an in-home estimate (confirms it to
//!   the customer and schedules reminders)
//! - `GET /v1/events/search` → find events
//! - `GET /v1/events/{id}/details` → comments, files, attachments, history
//! - `/v1/events/{id}/comments`, `/photos`, `/attachments` → add and remove
//!
//! Replaces the Remix `api.events`, `api.events.search`,
//! `api.event-details`, `api.event-attachments` and `api.event-images` routes.

pub mod details;
pub mod estimate;
pub mod history;
pub mod notify;
pub mod reminders;
pub mod writes;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(writes::create))
        .routes(routes!(writes::update, writes::delete))
        .routes(routes!(estimate::book))
        .routes(routes!(details::search))
        .routes(routes!(details::details))
        .routes(routes!(details::add_comment))
        .routes(routes!(details::edit_comment, details::delete_comment))
        .routes(routes!(details::add_photos))
        .routes(routes!(details::delete_photo))
        .routes(routes!(details::add_attachments))
        .routes(routes!(details::delete_attachment))
}
