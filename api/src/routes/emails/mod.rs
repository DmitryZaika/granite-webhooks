//! Emails: the caller's mailbox and conversations.
//!
//! - `GET  /v1/emails/threads`                    → a page of a folder (inbox, sent, trash, drafts;
//!   `scope=admin`: the whole company's mail)
//! - `GET  /v1/emails/folder-counts`              → unread inbox, trash and draft counts
//! - `GET  /v1/emails/unread`                     → the inbox badge
//! - `GET  /v1/deals/{id}/emails`                → a deal's email history
//! - `GET  /v1/emails/search-suggestions`         → typeahead of the search box
//! - `GET  /v1/emails/threads/{thread_id}`        → one conversation
//! - `POST /v1/emails/threads/{thread_id}/read`   → mark an opened conversation read
//! - `POST /v1/emails/threads/{mark-read,mark-unread,delete,restore,purge}` → bulk changes
//! - `POST /v1/emails`                            → send (or schedule) an email
//! - `POST /v1/email-attachments/{id}/deal`      → save an attachment to a deal
//! - `GET  /v1/deals/file-urls`                   → file URLs of a deal or a customer's deals
//! - `GET  /v1/emails/{id}/images`                 → an email's pictures, for the photo viewer
//! - `POST /v1/emails/drafts`, `GET|PUT|DELETE /v1/emails/drafts/{id}` → the caller's drafts
//!
//! Replaces the Remix `employee.emails*` and `admin.emails*` loaders and `api.emails.*` routes.
//! The `webhooks` Lambda builds and sends the MIME message (it owns SES).

mod admin_list;
pub mod attachment_token;
mod attachments;
mod conversation;
pub mod deal_history;
mod drafts;
mod handlers;
mod list;
mod names;
pub mod schemas;
pub mod send;
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
        .routes(routes!(send::send))
        .routes(routes!(drafts::create_draft))
        .routes(routes!(attachments::add_to_deal))
        .routes(routes!(attachments::deal_file_urls))
        .routes(routes!(attachments::email_images))
        .routes(routes!(conversation::conversation))
        .routes(routes!(
            drafts::get_draft,
            drafts::update_draft,
            drafts::delete_draft
        ))
}
