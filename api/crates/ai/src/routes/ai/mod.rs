//! AI helpers for writing messages.
//!
//! - `POST /v1/ai/improve` → rewrite a draft email or text
//! - `POST /v1/ai/call-note` → a CRM note from a call transcript
//! - `POST /v1/ai/call-activities` → follow-up activities from a call transcript
//! - `POST /v1/ai/email-template` → an email template from a description
//! - `POST /v1/ai/invoice-edit` → a QuickBooks invoice edit from an instruction
//! - `POST /v1/ai/signature-fields` → what each blank of a contract PDF is for
//! - `POST /v1/ai/agentic-questions`, `GET .../{id}` → questions for the agent,
//!   and `POST .../{id}/agent-token` → the agent's read-only token
//! - `GET /v1/ai/design/{pictures,deals}` → what the AI design tool picks from
//! - `GET /v1/ai/design/stones`, `POST /v1/ai/design` → AI Design's stones and
//!   the kitchen redrawn with them (streamed)
//! - `POST /v1/ai/conversation-reply` → the AI Reply button's draft
//! - `GET /v1/ai/compose/inventory-url`, `POST /v1/ai/compose/{autocomplete,learn}`
//!   → the composer's inline autocomplete and what the rep's edits teach it
//! - `POST /v1/ai/first-contact-email` → the first email to a new lead
//! - `POST /v1/ai/chat` → the AI chat's answer, streamed (server-sent events)
//! - `POST /v1/ai/crm-manual-feedback` → a thumbs-down on a CRM manual answer
//! - `GET /v1/ai/promises`, `PATCH /v1/ai/promises/{id}` → what the rep
//!   promised customers, and closing a promise
//!
//! Replaces the Remix `api.aiImprove.email` and `api.aiSummarize.callNote` routes.

pub mod agentic;
pub mod call_activity;
pub mod call_activity_rules;
pub mod call_note;
pub mod chat;
pub mod compose;
pub mod compose_catalog;
pub mod conversation_reply;
pub mod design;
pub mod design_render;
pub mod email_template;
pub mod first_contact;
pub mod improve;
pub mod invoice_edit;
pub mod manual_feedback;
pub mod promises;
pub mod reply_context;
pub mod signature_fields;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::extract::DefaultBodyLimit;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(improve::improve))
        .routes(routes!(call_note::call_note))
        .routes(routes!(call_activity::call_activities))
        .routes(routes!(email_template::email_template))
        .routes(routes!(invoice_edit::invoice_edit))
        .routes(routes!(signature_fields::detect))
        .routes(routes!(agentic::ask))
        .routes(routes!(agentic::job))
        .routes(routes!(agentic::agent_token))
        .routes(routes!(design::pictures))
        .routes(routes!(design::deals))
        .routes(routes!(design_render::stones))
        .merge(
            // A kitchen photo is more than axum's default 2 MB.
            OpenApiRouter::new()
                .routes(routes!(design_render::design))
                .layer(DefaultBodyLimit::max(
                    crate::kitchen_design::MAX_DESIGN_BODY_BYTES,
                )),
        )
        .routes(routes!(compose::inventory_url))
        .routes(routes!(compose::autocomplete))
        .routes(routes!(compose::learn))
        .routes(routes!(first_contact::first_contact_email))
        .routes(routes!(conversation_reply::conversation_reply))
        .merge(
            // A question with pictures is more than axum's default 2 MB.
            OpenApiRouter::new()
                .routes(routes!(chat::ask))
                .layer(DefaultBodyLimit::max(chat::MAX_CHAT_BODY_BYTES)),
        )
        .routes(routes!(manual_feedback::feedback))
        .routes(routes!(promises::list))
        .routes(routes!(promises::update))
}
