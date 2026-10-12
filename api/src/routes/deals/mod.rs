//! Deals: the sales pipeline board, deals, their activities and notes.
//!
//! - `GET    /v1/deal-groups`                     → pipelines, with overdue flags
//! - `GET    /v1/deal-lists`                      → stages (of a group, or all)
//! - `GET    /v1/deal-lists/{id}/recipients`      → emails of the caller's customers in a stage
//! - `GET    /v1/deals`                           → the caller's board cards of a group
//! - `POST   /v1/deals`                           → create (or reuse the open deal)
//! - `POST   /v1/deals/reorder`                   → save a drag on the board
//! - `GET    /v1/deals/by-customer`               → a customer's deals with previews
//! - `GET    /v1/deals/{id}`, `PATCH`, `DELETE`   → one deal
//! - `PUT    /v1/deals/{id}/outcome`              → won / lost / reopen
//! - `GET    /v1/deals/{id}/stage-history`        → stage visits
//! - `/v1/deals/{id}/activities[/{activity_id}]`  → to-dos (GET, POST, PATCH, DELETE)
//! - `/v1/deals/{id}/files`, `/documents/{id}`, `/files/{source}/{id}` → photos and documents
//! - `GET    /v1/deal-activities/suggestions`     → the caller's frequent activity names
//! - `/v1/deal-pipelines[/{id}[/stages|/stage-order]]`, `DELETE /v1/deal-stages/{id}` → admin pipeline setup
//! - `/v1/deals/{id}/notes[/{note_id}[/comments[/{comment_id}]]]` → notes and comments
//!
//! Replaces the Remix `employee.deals*` loaders/actions and `api.deal*` routes.

mod files;
mod handlers;
mod pipelines;
pub mod queries;
pub mod schemas;
pub mod service;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list_groups))
        .routes(routes!(handlers::list_lists))
        .routes(routes!(handlers::list_recipients))
        .routes(routes!(handlers::board, handlers::create))
        .routes(routes!(handlers::reorder))
        .routes(routes!(handlers::by_customer))
        .routes(routes!(handlers::get, handlers::update, handlers::delete))
        .routes(routes!(handlers::set_outcome))
        .routes(routes!(handlers::stage_history))
        .routes(routes!(
            handlers::list_activities,
            handlers::create_activity
        ))
        .routes(routes!(
            handlers::update_activity,
            handlers::delete_activity
        ))
        .routes(routes!(handlers::activity_suggestions))
        .routes(routes!(handlers::list_notes, handlers::create_note))
        .routes(routes!(handlers::update_note, handlers::delete_note))
        .routes(routes!(handlers::create_comment))
        .routes(routes!(handlers::delete_comment))
        .routes(routes!(files::list, files::attach_files))
        .routes(routes!(files::rename_document))
        .routes(routes!(files::delete_file))
        .routes(routes!(pipelines::list, pipelines::create))
        .routes(routes!(pipelines::update, pipelines::delete))
        .routes(routes!(pipelines::create_stage))
        .routes(routes!(pipelines::order_stages))
        .routes(routes!(pipelines::delete_stage))
}
