//! AI helpers for writing messages.
//!
//! - `POST /v1/ai/improve` → rewrite a draft email or text
//! - `POST /v1/ai/call-note` → a CRM note from a call transcript
//! - `POST /v1/ai/call-activities` → follow-up activities from a call transcript
//! - `POST /v1/ai/email-template` → an email template from a description
//! - `POST /v1/ai/invoice-edit` → a QuickBooks invoice edit from an instruction
//!
//! Replaces the Remix `api.aiImprove.email` and `api.aiSummarize.callNote` routes.

pub mod call_activity;
pub mod call_activity_rules;
pub mod call_note;
pub mod email_template;
pub mod improve;
pub mod invoice_edit;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(improve::improve))
        .routes(routes!(call_note::call_note))
        .routes(routes!(call_activity::call_activities))
        .routes(routes!(email_template::email_template))
        .routes(routes!(invoice_edit::invoice_edit))
}
