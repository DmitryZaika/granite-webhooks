//! AI helpers: the chat, writing and replying, design, call coaching,
//! instructions, agent tokens (api-ai).
//!
//! One file's tests: `cargo test -p api --test ai <file>::`.

#[path = "../common/mod.rs"]
mod common;

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
mod call_activity_rules;
mod call_coaching;
mod conversation_reply;
mod crm_manual_feedback;
mod instructions;
mod manual_feedback_reviews;
