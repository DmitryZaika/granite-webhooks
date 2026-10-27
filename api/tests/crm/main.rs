//! Customers, deals, emails, texts, calls, templates, uploads and stats
//! (api-crm).
//!
//! One file's tests: `cargo test -p api --test crm <file>::`.

#[path = "../common/mod.rs"]
mod common;

mod call_transcripts;
mod calls;
mod customer_history;
mod customer_lookup;
mod customer_phones;
mod customer_projects;
mod customer_writes;
mod customers;
mod deal_document_events;
mod deal_pipelines;
mod deals;
mod email_attachments;
mod email_conversation;
mod email_drafts;
mod email_signatures;
mod email_signatures_native;
mod emails;
mod media_previews;
mod send_email;
mod sms;
mod sms_send;
mod sms_templates;
mod stats;
mod stats_overview;
mod templates;
mod uploads;
mod widget_layout;
