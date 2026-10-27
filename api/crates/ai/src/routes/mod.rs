//! Route modules of this crate. The ones of the crates below are
//! re-exported, so every route module is at `crate::routes::<name>`.

pub use api_accounts::routes::*;
pub use api_followups::routes::*;
pub use api_inventory::routes::*;
pub use api_quotes::routes::*;

pub mod ai;
pub mod ai_usage;
pub mod call_coaching;
pub mod instructions;
pub mod manual_feedback_reviews;
