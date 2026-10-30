//! The webhooks Lambda's foundation, below every other webhooks crate: the
//! database code (`crud`), request schemas, the auth extractors and the
//! Telegram bots (`axum_helpers::guards`), `PostHog`, `OpenAI`, the
//! request logger, and the test helpers of every crate's tests
//! (`tests`, with the `test-support` feature).
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

pub mod amazon;
pub mod amazonses;
pub mod axum_helpers;
pub mod cloudtalk;
pub mod crud;
pub mod libs;
pub mod middleware;
pub mod openai;
pub mod posthog;
pub mod ringcentral;
pub mod schemas;
pub mod telegram;
pub mod telnyx;
#[cfg(any(test, feature = "test-support"))]
pub mod tests;
