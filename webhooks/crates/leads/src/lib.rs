//! Leads: the forms and lead sources that post them (`webhooks`), the
//! Telegram bot that hands them to sales reps (`telegram`), the lead and
//! referral logic (`libs`) and the email templates (`template`).
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::option_if_let_else,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::missing_panics_doc,
    // Checking that every async fn's future is Send took a third of
    // clippy's time; axum and tokio::spawn require it where it matters.
    clippy::future_not_send
)]

use axum::Router;
use axum::routing::{delete, get, post};
use sqlx::MySqlPool;

// The crates below, at the paths they had in the single webhooks crate.
pub use webhooks_phone::*;

pub mod libs;
pub mod telegram;
pub mod template;
#[cfg(test)]
pub mod tests;
pub mod webhooks;

/// This crate's routes.
pub fn router() -> Router<MySqlPool> {
    use webhooks::receive as forms;
    Router::new()
        .route(
            "/wordpress-contact-form/{company_id}",
            post(forms::wordpress_contact_form),
        )
        .route(
            "/facebook-contact-form/{company_id}",
            post(forms::facebook_contact_form),
        )
        .route(
            "/v1/webhooks/new-lead-form/{company_id}",
            post(forms::new_lead_form),
        )
        .route(
            "/telegram/webhook",
            post(telegram::receive::webhook_handler),
        )
        .route(
            "/telegram/lead-messages/{company_id}/{customer_id}",
            delete(telegram::cleanup::delete_lead_telegram_messages),
        )
        .route(
            "/telegram/crm-notify",
            post(telegram::crm_notify::crm_notify_handler),
        )
        .route(
            "/telegram/notifications-notify",
            post(telegram::notifications_notify::notifications_notify_handler),
        )
        .route(
            "/template/variables/{company_id}/{user_id}",
            get(template::receive::get_template_variables),
        )
        .route(
            "/template/complete/{company_id}/{user_id}",
            post(template::receive::get_complete_template),
        )
}
