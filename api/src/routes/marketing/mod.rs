//! The marketing team's portal: the leads of one company they work for
//! (position "Marketing" in that company), how they did, and lead upkeep.
//!
//! - `GET    /v1/marketing/{company_id}/leads` → leads, their deals and stats
//! - `POST   /v1/marketing/{company_id}/leads` → add a lead
//! - `GET|PUT|DELETE /v1/marketing/{company_id}/leads/{id}` → one lead
//! - `GET    /v1/marketing/{company_id}/leads/{id}/info` → its card
//! - `GET    /v1/marketing/{company_id}/leads/{id}/projects` → its projects
//! - `GET|POST /v1/marketing/{company_id}/referral-sources`,
//!   `PATCH|DELETE .../{id}` → where leads come from
//! - `POST   /v1/marketing/{company_id}/uploads` → a picture for a new lead
//!
//! Replaces the Remix `external.marketing.$companyId.leads*` routes.

mod handlers;
mod leads;
mod sources;

use crate::auth::session::SessionUser;
use crate::error::ApiError;
use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

/// `positions.id` of the marketing team.
const MARKETING_POSITION: i32 = 7;

/// The caller works on marketing for `company_id`; 403 otherwise.
async fn require_marketer(
    state: &AppState,
    user: &SessionUser,
    company_id: i32,
) -> Result<(), ApiError> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM users_positions WHERE user_id = ? AND position_id = ? AND company_id = ? LIMIT 1",
    )
    .bind(user.id)
    .bind(MARKETING_POSITION)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::Forbidden)
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(leads::list, leads::create))
        .routes(routes!(leads::get, leads::update, leads::delete))
        .routes(routes!(handlers::info))
        .routes(routes!(handlers::projects))
        .routes(routes!(handlers::upload))
        .routes(routes!(sources::list, sources::create))
        .routes(routes!(sources::rename, sources::delete))
}
