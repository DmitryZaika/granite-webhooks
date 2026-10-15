//! The signed-in user's own numbers, as the employee dashboard shows them.
//!
//! - `GET /v1/me/stats/sales`           → deals won, lost, open and new this month
//! - `GET /v1/me/stats/deal-conversion` → won deals out of all, by customer source
//! - `GET /v1/me/stats/top-stones`      → stones they sold the most slabs of this month
//! - `GET /v1/me/stats/sent-emails`     → emails sent per month, last 3 months
//! - `GET /v1/me/stats/sent-sms`        → texts sent per month, last 3 months
//! - `GET /v1/stats/overview`           → the admin Statistics page ([`overview`])
//! - `GET|PUT /v1/stats/widget-layout`  → its widget layout ([`widget_layout`])
//!
//! Replaces `salesRepStats.server.ts` and `employeeDashboardMetrics.server.ts`.

mod handlers;
pub mod overview;
pub mod queries;
pub mod schemas;
pub mod widget_layout;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

/// The current month and the two before it.
pub const MESSAGE_MONTHS: u32 = 3;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::sales))
        .routes(routes!(handlers::deal_conversion))
        .routes(routes!(handlers::top_stones))
        .routes(routes!(handlers::sent_emails))
        .routes(routes!(handlers::sent_sms))
        .routes(routes!(overview::overview))
        .routes(routes!(
            widget_layout::get_layout,
            widget_layout::save_layout
        ))
}
