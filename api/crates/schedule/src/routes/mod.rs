//! Route modules of this crate. The ones of the crates below are
//! re-exported, so every route module is at `crate::routes::<name>`.

pub use api_crm::routes::*;

pub mod calendars;
pub mod events;
pub mod schedule;
