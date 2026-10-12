//! Message templates of the caller: email templates, the picker order of
//! email / SMS / quote templates, and filling `{{variables}}`.
//!
//! - `GET    /v1/email-templates`               → company + own templates, in the caller's order
//! - `POST   /v1/email-templates`               → create (company scope for admins)
//! - `PUT    /v1/email-templates/{id}`          → edit an own (or, admins, a company) template
//! - `DELETE /v1/email-templates/{id}`          → delete one
//! - `POST   /v1/email-templates/{id}/attachments`, `DELETE .../{attachment_id}` → its files
//! - `GET    /v1/me/template-order/{type}`      → automatic or custom order
//! - `PUT    /v1/me/template-order/{type}`      → save a drag-and-drop order
//! - `DELETE /v1/me/template-order/{type}`      → back to automatic order
//! - `POST   /v1/templates/fill`                → a template with its variables filled
//! - `GET|POST /v1/sms-templates`, `PUT|DELETE /v1/sms-templates/{id}` → SMS templates (see [`sms`])
//!
//! Replaces the Remix `api.email-templates*`, `api.sms.templates.order`,
//! `api.quote-email-templates.order` and `api.emailTemplate.fill` routes and
//! the `admin.automations.email-templates*` loaders and actions.

pub mod handlers;
pub mod order;
pub mod schemas;
mod sms;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list, handlers::create))
        .routes(routes!(handlers::update, handlers::delete))
        .routes(routes!(handlers::add_attachment))
        .routes(routes!(handlers::delete_attachment))
        .routes(routes!(
            handlers::get_order,
            handlers::save_order,
            handlers::reset_order
        ))
        .routes(routes!(handlers::fill))
        .routes(routes!(sms::list, sms::create))
        .routes(routes!(sms::update, sms::delete))
}
