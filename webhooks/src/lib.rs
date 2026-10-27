//! The webhooks Lambda: every route (`axum_helpers::axum_app`), from the
//! crates in `crates/`. The code is there:
//!
//! - `webhooks-core`: database code, schemas, auth, the Telegram bots,
//!   `PostHog`, the request logger, the test helpers
//! - `webhooks-email`: inbound email and the emails employees send
//! - `webhooks-phone`: `CloudTalk`, `RingCentral`, Telnyx, texts, Google
//! - `webhooks-leads`: lead forms, the Telegram lead bot, templates
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

pub mod axum_helpers;

/// Serves every route on the Lambda runtime until it stops.
///
/// Boxed, so that the runtime's code (generic over the router) compiles with
/// this crate, while the crates below are still compiling: a future that only
/// main.rs polls compiles with the binary, whose link-time optimization waits
/// for every crate.
pub fn run(
    pool: sqlx::MySqlPool,
) -> std::pin::Pin<Box<dyn Future<Output = Result<(), lambda_http::Error>>>> {
    Box::pin(lambda_http::run(axum_helpers::axum_app::new_main_app(pool)))
}
