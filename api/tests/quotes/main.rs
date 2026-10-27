//! Quotes, online quotes and the portal, signatures, QuickBooks and commissions
//! (api-quotes).
//!
//! One file's tests: `cargo test -p api --test quotes <file>::`.

#[path = "../common/mod.rs"]
mod common;

mod commissions;
mod customer_view;
mod entitlements_quotes;
mod entitlements_signatures_qbo;
mod invoice_queue;
mod online_quotes;
mod quickbooks;
mod quote_editor;
mod quote_forms;
mod quote_presets;
mod quote_price_lists;
mod quote_pricing_parity;
mod quotes;
mod sent_signature_requests;
mod signature_documents;
mod signature_fields;
mod signature_reminders;
mod signature_requests;
mod signing;
mod signwell_webhook;
