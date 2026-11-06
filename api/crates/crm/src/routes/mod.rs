//! Route modules of this crate. The ones of the crates below are
//! re-exported, so every route module is at `crate::routes::<name>`.

pub use api_core::routes::*;

pub mod calls;
pub mod customers;
pub mod deals;
pub mod emails;
pub mod projects;
pub mod sms;
pub mod stats;
pub mod templates;
pub mod uploads;
