//! Helpers and stand-ins for the tests of every webhooks crate.
//!
//! Built for this crate's tests and, through the `test-support` feature that
//! the other crates' dev-dependencies turn on, for theirs; never in the Lambda.

pub mod cloudtalk;
pub mod deals;
pub mod ringcentral;
pub mod stub_http;
pub mod telegram;
pub mod utils;
