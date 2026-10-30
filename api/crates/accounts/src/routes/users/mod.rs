//! Other users of the session user's company.
//!
//! - `GET /v1/users/sales-reps` → live users of the company holding the sales rep position
//! - `GET /v1/users/employees` → the company's live employees
//! - `GET /v1/users/activity` → the last 90 days of what users did (admin)
//! - `GET /v1/users/call-activity` → reps' `CloudTalk`, `RingCentral` and Granite Phone calls (admin)
//! - `/v1/admin/users*` → managing users (see [`manage`])
//!
//! Replaces the Remix `app/routes/api.sales-reps.tsx` loader.

pub mod activity;
pub mod call_activity;
mod handlers;
pub mod manage;
pub mod queries;
pub mod texting_history;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::sales_reps))
        .routes(routes!(handlers::employees))
        .routes(routes!(activity::activity))
        .routes(routes!(call_activity::call_activity))
        .routes(routes!(manage::list, manage::create))
        .routes(routes!(manage::get, manage::update, manage::delete))
        .routes(routes!(manage::invite))
}
