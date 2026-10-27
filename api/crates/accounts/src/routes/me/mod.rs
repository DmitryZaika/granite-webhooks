//! The signed-in user.
//!
//! - `GET /v1/me`           → effective session user (no extra query beyond auth)
//! - `GET`/`PUT /v1/me/account` → My Account (see [`account`])
//! - `GET /v1/me/positions` → position ids in the company the request acts in
//! - `GET /v1/me/sales-goal` → monthly sales goal; `PUT` sets it
//! - `POST /v1/me/telegram/leads|notifications` → link a bot (Telegram login widget)
//! - `GET /v1/me/telegram`   → Telegram link status per bot (checked with Telegram)
//! - `GET`/`PUT /v1/me/notification-preferences` → Telegram alert settings
//! - `POST /v1/me/onboarding-feedback` → rate the guided tours (once)
//! - `PUT /v1/me/sidebar-pin` → keep the desktop sidebar open
//! - `GET /v1/me/marketing-companies` → companies the user markets for
//! - `/v1/me/todos`, `/v1/me/todo-order` → the to-do list (see [`todos`])

pub mod account;
pub mod handlers;
pub mod queries;
pub mod schemas;
mod todos;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::me))
        .routes(routes!(account::account, account::update_account))
        .routes(routes!(handlers::positions))
        .routes(routes!(handlers::sales_goal, handlers::set_sales_goal))
        .routes(routes!(handlers::telegram))
        .routes(routes!(handlers::link_leads_telegram))
        .routes(routes!(handlers::link_notifications_telegram))
        .routes(routes!(handlers::set_sidebar_pin))
        .routes(routes!(handlers::onboarding_feedback))
        .routes(routes!(
            handlers::notification_preferences,
            handlers::set_notification_preferences
        ))
        .routes(routes!(handlers::marketing_companies))
        .routes(routes!(todos::list, todos::create))
        .routes(routes!(todos::update, todos::delete))
        .routes(routes!(todos::set_done))
        .routes(routes!(todos::delete_done))
        .routes(routes!(todos::order))
}
