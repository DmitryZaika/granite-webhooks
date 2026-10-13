//! The AI follow-up engine.
//!
//! What the CRM knows about a customer's conversation (facts, contact
//! pressure, cadence) and the messages the AI writes from it: a port of the
//! CRM's TypeScript engine (`smsFlowAi*`, `aiFollowup*`,
//! `customerIntelligence/*`), checked against it output for output.
// Arithmetic stays as JS evaluates it: no fused multiply-add, and
// `max`/`min` rather than `clamp` (which differs on NaN). The control flow
// mirrors the TypeScript branch for branch so the two stay reviewable side
// by side, hence the structural lints below.
#![allow(
    clippy::suboptimal_flops,
    clippy::manual_clamp,
    clippy::manual_midpoint,
    clippy::option_if_let_else,
    clippy::collapsible_if,
    clippy::items_after_statements,
    clippy::nonminimal_bool
)]

pub mod attribution;
pub mod cadence;
pub mod classify;
pub mod contact_resume;
pub mod conversation;
pub mod crm_hold;
pub mod dates;
pub mod facts;
pub mod instruction;
pub mod intelligence;
pub mod intent;
pub mod js;
pub mod lifecycle;
pub mod phone;
pub mod pressure;
pub mod prompt;
pub mod prompt_texts;
pub mod send_window;
pub mod store_time;
pub mod style;
pub mod timeline;
