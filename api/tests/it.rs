//! Every integration test of `api` as a module of one test binary: after a
//! change in `src/`, cargo compiles and links this one binary instead of one
//! per file. A new `tests/<name>.rs` needs its `mod <name>;` below
//! (`every_test_file_is_a_module` fails without it). `server_errors.rs` sets
//! process-wide PostHog variables, so it stays a binary of its own (see
//! `Cargo.toml`).
//!
//! One file's tests: `cargo test -p api --test it <name>::`.

mod common;
mod phone_common;

mod agent_tokens;
mod agentic_questions;
mod ai;
mod ai_chat;
mod ai_compose;
mod ai_design;
mod ai_design_render;
mod ai_drafts;
mod ai_first_contact;
mod ai_usage;
mod app_shell;
mod attachable;
mod auth;
mod auth_routes;
mod billing;
mod billing_stripe_live;
mod bug_reports;
mod calendars;
mod call_activity_rules;
mod call_coaching;
mod call_transcripts;
mod calls;
mod checklists;
mod commissions;
mod company;
mod conversation_reply;
mod crm_manual_feedback;
mod customer_history;
mod customer_lookup;
mod customer_phones;
mod customer_projects;
mod customer_sources;
mod customer_view;
mod customer_writes;
mod customers;
mod dashboard;
mod db_pool;
mod deal_document_events;
mod deal_pipelines;
mod deals;
mod delayed;
mod device_crm_routes;
mod documents;
mod drive_miles;
mod email_attachments;
mod email_conversation;
mod email_drafts;
mod email_signatures;
mod email_signatures_native;
mod emails;
mod estimates;
mod events;
mod faucets;
mod file_ownership;
mod fixture_models;
mod followup_ai_cadence;
mod followup_call_check;
mod followup_flows;
mod followup_gather;
mod followup_generation;
mod followup_instruction;
mod followup_settings;
mod followup_timeline;
mod image_library;
mod installed_images;
mod instructions;
mod integrations;
mod invoice_queue;
mod jobs;
mod leaderboard;
mod maintenance;
mod manage_users;
mod manual_feedback_reviews;
mod marketing;
mod me;
mod me_account;
mod media_previews;
mod notifications;
mod onboarding;
mod online_quotes;
mod openapi;
mod phone_admin;
mod phone_auth;
mod phone_bootstrap;
mod phone_call_settings;
mod phone_calls;
mod phone_connect;
mod phone_contacts;
mod phone_devices;
mod phone_email_push;
mod phone_jobs;
mod phone_porting;
mod phone_revocation;
mod phone_settings;
mod phone_telnyx_dial;
mod phone_telnyx_inbound;
mod phone_telnyx_outbound;
mod phone_telnyx_recording;
mod phone_telnyx_via_cell;
mod phone_texts;
mod phone_web;
mod phone_web_recording;
mod places;
mod posthog_autofix_callback;
mod posthog_autofix_intake;
mod public;
mod questionnaires;
mod quickbooks;
mod quote_editor;
mod quote_forms;
mod quote_presets;
mod quote_price_lists;
mod quote_pricing_parity;
mod quotes;
mod reports;
mod sale_contract;
mod sales;
mod schedule;
mod schedule_board;
mod schedule_notifications;
mod send_email;
mod sent_signature_requests;
mod shop;
mod signature_documents;
mod signature_fields;
mod signature_reminders;
mod signature_requests;
mod signing;
mod signwell_webhook;
mod sinks;
mod sms;
mod sms_flows;
mod sms_followups_job;
mod sms_send;
mod sms_templates;
mod stats;
mod stats_overview;
mod stone_admin;
mod stone_search;
mod stone_slabs;
mod stones;
mod superuser;
mod superuser_companies;
mod supplier_files;
mod suppliers;
mod supports;
mod survey_results;
mod teach_mode;
mod telnyx_10dlc;
mod telnyx_calls;
mod telnyx_sms;
mod templates;
mod todos;
mod transaction_slabs;
mod transactions;
mod uploads;
mod user_call_activity;
mod users;
mod users_activity;
mod widget_layout;

#[test]
fn every_test_file_is_a_module() {
    let listed = include_str!("it.rs");
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut missing: Vec<String> = std::fs::read_dir(tests)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
        .filter(|name| name != "it" && name != "server_errors")
        .filter(|name| !listed.contains(&format!("\nmod {name};\n")))
        .collect();
    missing.sort();
    assert!(missing.is_empty(), "list these in tests/it.rs: {missing:?}");
}
