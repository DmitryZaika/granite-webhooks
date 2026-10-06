//! Other users of the session user's company.
//!
//! - `GET /v1/users/sales-reps` → live users of the company holding the sales rep position
//!
//! Replaces the Remix `app/routes/api.sales-reps.tsx` loader.

mod handlers;
pub mod queries;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(handlers::sales_reps))
}
