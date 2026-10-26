//! The signed-in user's employee dashboard.
//!
//! - `GET  /v1/me/dashboard/widgets`       → saved widget layout
//! - `PUT  /v1/me/dashboard/widgets`       → save the layout (records adds/removes)
//! - `POST /v1/me/dashboard/widget-events` → record a widget use (analytics)
//!
//! Replaces the Remix dashboard action's `set-widgets` intent, the layout
//! cookie, `api.employee.dashboard-widgets.sync` and `api.widget-analytics`.
//! Which widgets are offered, and the defaults, stay in the frontend
//! (`app/utils/employeeDashboardWidgets.ts`).

mod handlers;
pub mod queries;
pub mod schemas;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::layout, handlers::save_layout))
        .routes(routes!(handlers::record_widget_event))
}
