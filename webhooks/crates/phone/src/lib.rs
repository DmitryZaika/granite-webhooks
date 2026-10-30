//! Calls and texts: `CloudTalk`, `RingCentral` and Telnyx's webhooks, the
//! CRM contact sync with them, texts employees send (`outbound::sms`) and
//! Google address lookups.
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

pub mod cloudtalk;
pub mod google;
pub mod outbound;
pub mod ringcentral;
pub mod telnyx;
#[cfg(test)]
pub mod tests;

/// This crate's routes.
pub fn router() -> Router<MySqlPool> {
    use cloudtalk::receive as ct;
    use ringcentral::receive as rc;
    Router::new()
        .route("/sms/send", post(outbound::sms::send_handler))
        .route("/cloudtalk/sms", post(ct::sms_received_unscoped))
        .route("/cloudtalk/sms/{company_id}", post(ct::sms_received))
        .route("/cloudtalk/sms/sent/{company_id}", post(ct::sms_sent))
        .route("/cloudtalk/call/{company_id}", post(ct::call_received))
        .route(
            "/cloudtalk/sync/{company_id}/{customer_id}",
            post(ct::sync_cloudtalk).delete(ct::unsync_cloudtalk),
        )
        .merge(ringcentral_webhooks())
        .route(
            "/ringcentral/sync/{company_id}/{customer_id}",
            post(rc::sync_ringcentral).delete(rc::unsync_ringcentral),
        )
        .route(
            "/telnyx/event-progress",
            post(telnyx::receive::event_progress),
        )
        .route(
            "/google/address-autocomplete",
            post(google::receive::address_information),
        )
}

/// The routes `RingCentral` calls, which answer its subscription handshake
/// before anything else.
fn ringcentral_webhooks() -> Router<MySqlPool> {
    use ringcentral::receive as rc;
    Router::new()
        .route("/ringcentral/sms/{company_id}", post(rc::sms_received))
        .route("/ringcentral/sms/sent/{company_id}", post(rc::sms_sent))
        .route(
            "/ringcentral/notify/{company_id}",
            post(rc::sms_notification),
        )
        .route("/ringcentral/call/{company_id}", post(rc::call_received))
        .route_layer(axum::middleware::from_fn(rc::handshake))
}
