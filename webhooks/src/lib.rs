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
pub mod google;
pub mod libs;
pub mod middleware;
pub mod openai;
pub mod outbound;
pub mod posthog;
pub mod ringcentral;
pub mod schemas;
pub mod telegram;
pub mod telnyx;
pub mod template;
pub mod tests;
pub mod webhooks;
