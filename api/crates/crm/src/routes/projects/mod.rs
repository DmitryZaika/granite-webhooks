//! Projects: a sold job in production, from template to paid. A deal that is
//! won starts its project (Awaiting Template); a transaction made for the
//! customer links to it; Installed and Paid & Completed follow the
//! transaction. Off until a superuser turns Projects on for the company.
//!
//! - `GET    /v1/projects`                         → the caller's board
//! - `GET    /v1/projects/{id}`                    → one project's page
//! - `POST   /v1/projects/{id}/actions`            → move, link, pay, hold, cancel, archive, correct
//! - `GET`/`POST /v1/deals/{id}/project`            → a deal's project; start it by hand
//! - `/v1/projects/{id}/tasks[/{task_id}]`         → tasks for anyone in the company (POST, PATCH, DELETE)
//! - `/v1/projects/{id}/notes[/{note_id}]`         → notes (POST, DELETE)
//! - `/v1/projects/{id}/files[/{file_id}]`         → files from `POST /v1/uploads` (POST, DELETE)
//!
//! Who sees what is in [`access`].

pub mod access;
mod handlers;
pub mod schemas;
pub mod service;
mod tasks;

pub use tasks::{TASK_ASSIGNED, TASK_DUE};

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::board))
        .routes(routes!(handlers::get))
        .routes(routes!(handlers::act))
        .routes(routes!(handlers::deal_project, handlers::convert_deal))
        .routes(routes!(tasks::create_task))
        .routes(routes!(tasks::update_task, tasks::delete_task))
        .routes(routes!(tasks::create_note))
        .routes(routes!(tasks::delete_note))
        .routes(routes!(tasks::attach_files))
        .routes(routes!(tasks::delete_file))
}
