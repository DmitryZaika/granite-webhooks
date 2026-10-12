use crate::serde_helpers::js_date;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::{IntoParams, ToSchema};

/// A mailbox folder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EmailFolder {
    /// Mail addressed to the caller (to/cc), customers' and coworkers'.
    #[default]
    Inbox,
    /// Mail the caller sent.
    Sent,
    /// Deleted inbox mail.
    Trash,
    /// The caller's unsent drafts.
    Drafts,
}

/// Query of `GET /v1/emails/threads`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Default `inbox`.
    #[param(inline)]
    pub folder: Option<EmailFolder>,
    /// 1-based page of 25 threads. Default 1.
    pub page: Option<i64>,
    /// Words to find in subject, text and addresses, or a customer name
    /// (whose addresses then match).
    pub search: Option<String>,
}

/// A message of a listed thread (every message of the page's threads).
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct MailboxRow {
    /// Message id.
    pub id: i32,
    /// Conversation id (UUID).
    pub thread_id: Option<String>,
    /// Subject.
    pub subject: Option<String>,
    /// Plain text body.
    pub body: String,
    /// When it was sent.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub sent_at: Option<DateTime<Utc>>,
    /// From header (`Name <address>` or an address).
    pub sender_email: Option<String>,
    /// First To header.
    pub receiver_email: Option<String>,
    /// Sending user, null for mail from outside.
    pub sender_user_id: Option<i32>,
    /// When an employee opened it; null while unread.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub employee_read_at: Option<DateTime<Utc>>,
    /// Unsent draft.
    pub is_draft: bool,
    /// When the recipient last opened it (sent mail only).
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub client_read_at: Option<DateTime<Utc>>,
    /// The message has attachments.
    pub has_attachments: bool,
    /// Some message of the conversation has attachments.
    pub thread_has_attachments: bool,
    /// Sender's name: user, customer, or the name in a header.
    pub sender_name: Option<String>,
    /// Receiver's name, the same way.
    pub receiver_name: Option<String>,
    /// Inbox only: the time the thread is ranked by (its latest message to the caller).
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub inbox_sort_at: Option<DateTime<Utc>>,
}

/// One page of a folder.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MailboxPage {
    /// Every message of the page's threads, threads newest first.
    pub emails: Vec<MailboxRow>,
    /// Threads in the folder (with the search).
    pub total_count: i64,
    /// This page.
    pub page: i64,
    /// Threads per page.
    pub page_size: i64,
}

/// Thread counts of the caller's folders.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FolderCounts {
    /// Unread inbox threads.
    pub inbox_unread: i64,
    /// Threads in the trash.
    pub trash: i64,
    /// Draft threads.
    pub drafts: i64,
}

/// The inbox badge.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UnreadCount {
    /// Unread inbox threads; 0 when the company has no inbound email.
    pub count: i64,
    /// Newest unread message id, to notice new mail between polls.
    pub latest_inbound_id: Option<i64>,
}

/// Query of `GET /v1/emails/threads/{thread_id}`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ThreadQuery {
    /// `trash` shows the deleted messages of the thread instead.
    #[param(inline)]
    pub folder: Option<EmailFolder>,
    /// Open it on this deal: only the deal's (and unassigned) messages, the
    /// deal's customer, and an empty conversation for a new thread id.
    pub deal_id: Option<i64>,
}

/// Who the conversation is with.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ThreadCustomer {
    /// Customer id, when the address belongs to one.
    pub id: Option<i32>,
    /// Customer, header or coworker name; the address, or `Customer`.
    pub name: String,
    /// The address to reply to.
    pub email: String,
}

/// A file attached to a message.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct Attachment {
    /// Attachment id.
    pub id: i32,
    /// Message it belongs to.
    pub email_id: i32,
    /// MIME type (`application`).
    pub content_type: String,
    /// MIME subtype (`pdf`).
    pub content_subtype: Option<String>,
    /// File name.
    pub filename: String,
    /// Stored URL.
    pub url: String,
    /// A link the browser can open, for private files (valid an hour).
    #[sqlx(skip)]
    pub signed_url: Option<String>,
    /// Status of an e-signature request made from it: `draft`, `sent`,
    /// `viewed`, `signed`, `declined`, `expired`; null without one.
    #[sqlx(skip)]
    pub signature_status: Option<String>,
}

/// A message of a thread.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ThreadMessage {
    /// Message id.
    pub id: i32,
    /// Subject.
    pub subject: Option<String>,
    /// Plain text body.
    pub body: String,
    /// HTML body, when the mail had one.
    pub html_body: Option<String>,
    /// When it was sent.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub sent_at: Option<DateTime<Utc>>,
    /// From header.
    pub sender_email: Option<String>,
    /// First To header.
    pub receiver_email: Option<String>,
    /// Receiving user, when known.
    pub receiver_user_id: Option<i32>,
    /// When an employee opened it.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub employee_read_at: Option<DateTime<Utc>>,
    /// Sending user; null for mail from outside.
    pub sender_user_id: Option<i32>,
    /// Email signature of the sending user.
    pub signature: Option<String>,
    /// When the recipient last opened it.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub read_at: Option<DateTime<Utc>>,
    /// Attachments.
    #[sqlx(skip)]
    pub attachments: Vec<Attachment>,
}

/// An address on a message (no BCC).
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct Participant {
    /// Message.
    pub email_id: i32,
    /// `from`, `to` or `cc`.
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    pub kind: String,
    /// Address, lowercased.
    pub email: String,
    /// Name from the header.
    pub display_name: Option<String>,
    /// The user it belongs to.
    pub user_id: Option<i32>,
    /// The customer it belongs to.
    pub customer_id: Option<i32>,
    /// That user's name.
    pub user_name: Option<String>,
    /// That customer's name.
    pub customer_name: Option<String>,
}

/// A conversation.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ThreadDetail {
    /// Conversation id.
    pub thread_id: String,
    /// Deal the conversation is on.
    pub deal_id: Option<i64>,
    /// Who it is with.
    pub customer: ThreadCustomer,
    /// Subject of the first message.
    pub subject: Option<String>,
    /// The caller's email signature.
    pub current_user_signature: Option<String>,
    /// The address the caller's mail leaves from (theirs, or the shared one).
    pub sending_email: String,
    /// Messages, oldest first (on a deal: the deal's and unassigned ones).
    pub messages: Vec<ThreadMessage>,
    /// Addresses of every message, in header order (no BCC).
    pub participants: Vec<Participant>,
    /// Every address of the customer, primary first (empty when unknown).
    pub customer_addresses: Vec<String>,
}

/// Body of thread writes.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ThreadIds {
    /// Conversation ids (only the caller's own threads are changed).
    pub thread_ids: Vec<String>,
}

/// Result of `POST /v1/emails/threads/purge`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Purged {
    /// Messages erased.
    pub purged: usize,
}

/// Whose mail the suggestions search.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SuggestScope {
    /// The caller's own mailbox.
    #[default]
    Employee,
    /// The whole company's mail (admins only).
    Admin,
}

/// Query of `GET /v1/emails/search-suggestions`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SuggestQuery {
    /// What the user typed; fewer than 2 characters returns nothing.
    pub term: String,
    /// Default `inbox` (`drafts` is `inbox` for the admin scope).
    #[param(inline)]
    pub folder: Option<EmailFolder>,
    /// Default `employee`.
    #[param(inline)]
    pub scope: Option<SuggestScope>,
    /// Admin scope: only mail sent by or to this user.
    pub sales_rep: Option<i32>,
}

/// The newest matching message of a conversation.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct SuggestionRow {
    /// Conversation to open.
    pub thread_id: String,
    /// Subject.
    pub subject: Option<String>,
    /// Plain text body (the client makes the snippet).
    pub body: String,
    /// When it was sent.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub sent_at: Option<DateTime<Utc>>,
    /// From header.
    pub sender_email: Option<String>,
    /// To header.
    pub receiver_email: Option<String>,
    /// Sender's user or customer name.
    pub sender_name: Option<String>,
    /// Receiver's user or customer name.
    pub receiver_name: Option<String>,
}
