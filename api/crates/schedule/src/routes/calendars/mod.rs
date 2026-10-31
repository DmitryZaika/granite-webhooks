//! Schedule calendars of the company.
//!
//! - `GET /v1/calendars` → calendars on the caller's schedule
//! - `POST /v1/calendars`, `PUT`/`DELETE /v1/calendars/{id}`,
//!   `PUT /v1/calendars/{id}/hidden` → admins manage them
//!
//! Replaces `loadVisibleCalendars` (`app/utils/systemCalendars.server.ts`).

mod handlers;
pub mod queries;
pub mod schemas;
mod writes;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list, writes::create))
        .routes(routes!(writes::update, writes::delete))
        .routes(routes!(writes::set_hidden))
        .routes(routes!(writes::set_day_map))
        .routes(routes!(handlers::hidden_users))
        .routes(routes!(writes::set_hidden_for_user))
}
