//! The AI follow-up engine.
//!
//! What the CRM knows about a customer's conversation (facts, contact
//! pressure, cadence) and the messages the AI writes from it: a port of the
//! CRM's TypeScript engine (`smsFlowAi*`, `aiFollowup*`,
//! `customerIntelligence/*`), checked against it output for output.
// Arithmetic stays as JS evaluates it: no fused multiply-add.
#![allow(clippy::suboptimal_flops)]

pub mod attribution;
pub mod classify;
pub mod contact_resume;
pub mod conversation;
pub mod crm_hold;
pub mod dates;
pub mod facts;
pub mod intent;
pub mod js;
pub mod lifecycle;
pub mod phone;
pub mod pressure;
pub mod send_window;
pub mod store_time;
pub mod timeline;
