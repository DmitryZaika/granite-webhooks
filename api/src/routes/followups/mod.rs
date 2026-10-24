//! AI follow-ups: the company rule the follow-up writer obeys.
//!
//! - `GET /v1/followups/instruction` → the company's AI follow-up instruction
//! - `PUT /v1/followups/instruction` → save it (empty deletes it)
//! - `GET|PUT /v1/followups/offer` → the company's current offer the writer may mention
//! - `GET|PUT /v1/followups/signature` → the caller's sign-off under AI texts
//! - `GET /v1/followups/timeline` → the conversation the writer reads
//! - `GET|POST /v1/customers/{id}/flow`, `GET|POST /v1/sms/{provider}/threads/{phone}/flow`
//!   → the flow running for a customer or thread, and the rep's actions on it
//! - `POST /v1/followups/call-check` → after an outbound call, did the customer talk
//! - `POST /v1/followups/ai-cadence/preview`, `/regenerate`, `POST /v1/followups/ai-cadence`
//!   → write, rewrite and start an AI follow-up
//!
//! Replaces the Remix `api.followups.ai-instruction` and
//! `api.followups.timeline` routes, and the flow routes (see [`flow`]).

pub mod ai_cadence;
pub mod call_check;
pub mod flow;
pub mod instruction;
pub mod offer;
pub mod signature;
pub mod timeline;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(instruction::get, instruction::put))
        .routes(routes!(offer::get, offer::put))
        .routes(routes!(signature::get, signature::put))
        .routes(routes!(timeline::timeline))
        .routes(routes!(flow::get_customer_flow, flow::act_on_customer_flow))
        .routes(routes!(flow::get_thread_flow, flow::act_on_thread_flow))
        .routes(routes!(call_check::call_check))
        .routes(routes!(ai_cadence::preview))
        .routes(routes!(ai_cadence::regenerate))
        .routes(routes!(ai_cadence::start))
}
