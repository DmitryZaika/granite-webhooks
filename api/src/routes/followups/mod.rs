//! AI follow-ups: the company rule the follow-up writer obeys.
//!
//! - `GET /v1/followups/instruction` → the company's AI follow-up instruction
//! - `PUT /v1/followups/instruction` → save it (empty deletes it)
//! - `GET /v1/followups/timeline` → the conversation the writer reads
//! - `GET|POST /v1/customers/{id}/flow`, `GET|POST /v1/sms/{provider}/threads/{phone}/flow`
//!   → the flow running for a customer or thread, and the rep's actions on it
//! - `POST /v1/followups/call-check` → after an outbound call, did the customer talk
//!
//! Replaces the Remix `api.followups.ai-instruction` and
//! `api.followups.timeline` routes, and the flow routes (see [`flow`]).

pub mod call_check;
pub mod flow;
pub mod instruction;
pub mod timeline;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(instruction::get, instruction::put))
        .routes(routes!(timeline::timeline))
        .routes(routes!(flow::get_customer_flow, flow::act_on_customer_flow))
        .routes(routes!(flow::get_thread_flow, flow::act_on_thread_flow))
        .routes(routes!(call_check::call_check))
}
