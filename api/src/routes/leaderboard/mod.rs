//! The company's points leaderboard.
//!
//! - `GET /v1/points-leaderboard?period=all|month` → reps ranked by points
//!
//! Replaces `pointsLeaderboard.server.ts` (and the call/SMS part of
//! `callSmsPoints.server.ts`) for the employee dashboard and the Remix
//! `api.employee.points-leaderboard` route.

mod handlers;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(handlers::points_leaderboard))
}
