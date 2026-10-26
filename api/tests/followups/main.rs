//! AI follow-ups and SMS flows (api-followups).
//!
//! One file's tests: `cargo test -p api --test followups <file>::`.

#[path = "../common/mod.rs"]
mod common;

mod followup_ai_cadence;
mod followup_call_check;
mod followup_flows;
mod followup_gather;
mod followup_generation;
mod followup_instruction;
mod followup_settings;
mod followup_timeline;
mod sms_flows;
mod sms_followups_job;
