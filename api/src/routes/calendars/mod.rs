//! Schedule calendars of the company.
//!
//! - `GET /v1/calendars` → calendars on the caller's schedule
//!
//! Replaces `loadVisibleCalendars` (`app/utils/systemCalendars.server.ts`).

mod handlers;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(handlers::list))
}
