//! Route modules of this crate. The ones of the crates below are
//! re-exported, so every route module is at `crate::routes::<name>`.

pub use api_crm::routes::*;

pub mod attachable;
pub mod catalog;
pub mod documents;
pub mod faucets;
pub mod fixtures;
pub mod image_library;
pub mod places;
pub mod sales;
pub mod shop;
pub mod sinks;
pub mod slabs;
pub mod stones;
pub mod supplier_files;
pub mod suppliers;
pub mod supports;
pub mod transaction_slabs;
pub mod transactions;
