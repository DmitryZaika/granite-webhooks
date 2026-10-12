//! Other users of the session user's company.
//!
//! - `GET /v1/users/sales-reps` → live users of the company holding the sales rep position
//! - `GET /v1/users/call-activity` → reps' `CloudTalk` calls (admin)
//! - `/v1/admin/users*` → managing users (see [`manage`])
//!
//! Replaces the Remix `app/routes/api.sales-reps.tsx` loader.

pub mod call_activity;
mod handlers;
pub mod manage;
pub mod queries;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::sales_reps))
        .routes(routes!(call_activity::call_activity))
        .routes(routes!(manage::list, manage::create))
        .routes(routes!(manage::get, manage::update, manage::delete))
}
