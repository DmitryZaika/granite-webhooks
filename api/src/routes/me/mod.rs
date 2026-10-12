//! The signed-in user.
//!
//! - `GET /v1/me`           → effective session user (no extra query beyond auth)
//! - `GET /v1/me/positions` → position ids in the company the request acts in
//! - `GET /v1/me/sales-goal` → monthly sales goal; `PUT` sets it
//! - `GET /v1/me/telegram`   → Telegram link status per bot (checked with Telegram)
//! - `/v1/me/todos`, `/v1/me/todo-order` → the to-do list (see [`todos`])

mod handlers;
pub mod queries;
pub mod schemas;
mod todos;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::me))
        .routes(routes!(handlers::positions))
        .routes(routes!(handlers::sales_goal, handlers::set_sales_goal))
        .routes(routes!(handlers::telegram))
        .routes(routes!(todos::list, todos::create))
        .routes(routes!(todos::update, todos::delete))
        .routes(routes!(todos::set_done))
        .routes(routes!(todos::delete_done))
        .routes(routes!(todos::order))
}
