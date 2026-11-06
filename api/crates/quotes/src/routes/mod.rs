//! Route modules of this crate. The ones of the crates below are
//! re-exported, so every route module is at `crate::routes::<name>`.

pub use api_schedule::routes::*;

pub mod commissions;
pub mod customer_view;
pub mod feature_sales;
pub mod customer_view_extras;
pub mod invoice_queue;
pub mod invoices;
pub mod online_quotes;
pub mod quickbooks;
pub mod quote_editor;
pub mod quote_extras;
pub mod quote_forms;
pub mod quote_invoice_sync;
pub mod quote_lookups;
pub mod quote_portal;
pub mod quote_presets;
pub mod quote_price_lists;
pub mod quote_sale_events;
pub mod quotes;
pub mod signature_requests;
pub mod signatures;
pub mod signing;
pub mod signwell_webhook;
