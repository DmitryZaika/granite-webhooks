//! The api's foundation: [`state::AppState`] and the clients it holds (storage,
//! Telnyx, Stripe, OpenAI, QuickBooks, ...), errors, the authentication
//! extractors (`auth`) and the route macros (`openapi`). Every other api crate
//! builds on it.
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::missing_panics_doc,
    // Row structs mirror tinyint(1) columns.
    clippy::struct_excessive_bools,
    // Doc comments double as OpenAPI descriptions; product names like
    // CloudTalk are prose there, not code.
    clippy::doc_markdown
)]

pub mod ai;
pub mod app_origin;
pub mod auth;
pub mod billing;
pub mod db;
pub mod deliverability;
pub mod error;
pub mod extract;
pub mod html;
pub mod maps;
pub mod native_signing;
pub mod oauth_state;
pub mod openapi;
pub mod period;
pub mod phone_config;
pub mod posthog;
pub mod push;
pub mod qbo;
pub mod ringcentral_connect;
pub mod routes;
pub mod serde_helpers;
pub mod sigv4;
pub mod sms_provider;
pub mod sql;
pub mod state;
pub mod storage;
pub mod telegram;
pub mod telnyx;
pub mod transcribe;
pub mod voicemail;
pub mod webhooks_client;
