//! Emails: the caller's mailbox and conversations.
//!
//! - `GET  /v1/emails/threads`                    → a page of a folder (inbox, sent, trash, drafts)
//! - `GET  /v1/emails/folder-counts`              → unread inbox, trash and draft counts
//! - `GET  /v1/emails/unread`                     → the inbox badge
//! - `GET  /v1/deals/{id}/emails`                → a deal's email history
//! - `GET  /v1/emails/search-suggestions`         → typeahead of the search box
//! - `GET  /v1/emails/threads/{thread_id}`        → one conversation
//! - `POST /v1/emails/threads/{thread_id}/read`   → mark an opened conversation read
//! - `POST /v1/emails/threads/{mark-read,mark-unread,delete,restore,purge}` → bulk changes
//!
//! Replaces the Remix `employee.emails*` loaders and `api.emails.*` routes.
//! Sending mail lives in the `webhooks` Lambda (it owns SES).

pub mod deal_history;
mod handlers;
mod list;
mod names;
pub mod schemas;
pub mod sql;
pub mod thread;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list))
        .routes(routes!(handlers::counts))
        .routes(routes!(handlers::unread))
        .routes(routes!(handlers::get_thread))
        .routes(routes!(handlers::read_thread))
        .routes(routes!(handlers::mark_read))
        .routes(routes!(handlers::mark_unread))
        .routes(routes!(handlers::delete_threads))
        .routes(routes!(handlers::restore_threads))
        .routes(routes!(handlers::purge))
        .routes(routes!(handlers::suggest))
        .routes(routes!(deal_history::deal_emails))
}
