//! Email: the inbound intake SES hands over (`amazonses`) and the emails
//! employees send (`outbound`, `POST /email/send`).
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
use axum::routing::post;
use sqlx::MySqlPool;

// The crates below, at the paths they had in the single webhooks crate.
pub use webhooks_core::*;

pub mod amazonses;
pub mod outbound;
#[cfg(test)]
pub mod tests;

/// This crate's routes.
pub fn router() -> Router<MySqlPool> {
    Router::new()
        .route(
            "/ses/read-receipt",
            post(amazonses::routes::read_receipt_handler),
        )
        .route(
            "/ses/receive-email",
            post(amazonses::routes::receive_handler),
        )
        .route("/email/send", post(outbound::send_handler))
}
