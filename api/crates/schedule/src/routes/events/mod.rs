//! Schedule events.
//!
//! - `POST /v1/events` → add one
//! - `PUT /v1/events/{id}` → edit (re-times its appointment reminders)
//! - `DELETE /v1/events/{id}` → delete (cancels its reminders)
//! - `POST /v1/events/estimates` → book an in-home estimate (confirms it to
//!   the customer and schedules reminders)
//! - `POST /v1/events/{id}/confirmation` → text or email the customer a
//!   template now (in `schedule::notifications`)
//! - `GET /v1/events/search` → find events
//! - `GET /v1/events/{id}/details` → comments, files, attachments, history
//! - `/v1/events/{id}/comments`, `/photos`, `/attachments` → add and remove
//! - `/v1/events/{id}/project-notes` → notes on the event's sold project
//!
//! Replaces the Remix `api.events`, `api.events.search`,
//! `api.event-details`, `api.event-attachments` and `api.event-images` routes.

pub mod details;
pub mod estimate;
pub mod history;
pub mod notify;
pub mod project_notes;
pub mod reminders;
pub mod writes;

use crate::openapi::{OpenApiRouter, routes};
use crate::routes::schedule::notifications;
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(writes::create))
        .routes(routes!(writes::update, writes::delete))
        .routes(routes!(notifications::send_confirmation))
        .routes(routes!(estimate::book))
        .routes(routes!(details::search))
        .routes(routes!(details::details))
        .routes(routes!(details::add_comment))
        .routes(routes!(details::edit_comment, details::delete_comment))
        .routes(routes!(details::add_photos))
        .routes(routes!(details::delete_photo))
        .routes(routes!(details::add_attachments))
        .routes(routes!(details::delete_attachment))
        .routes(routes!(project_notes::list, project_notes::add))
        .routes(routes!(project_notes::delete))
}
