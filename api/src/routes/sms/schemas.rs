use super::scope::SmsScope;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Which way a message went, seen from the company.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Inbound,
    Outbound,
}

/// Query of `GET /v1/sms/{provider}/threads`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ThreadsQuery {
    /// Phone digits, message text, or a customer's or supplier's name.
    #[serde(default)]
    pub search: String,
    /// Page size, 1 to 100 (default 20).
    pub limit: Option<i64>,
    /// Threads to skip.
    pub offset: Option<i64>,
    /// `all` for the whole company (admins only).
    #[serde(default)]
    #[param(inline)]
    pub scope: SmsScope,
    /// With `scope=all`, one rep's conversations (their agent id).
    pub agent_id: Option<String>,
}

/// One conversation in the inbox.
#[derive(Debug, Serialize, ToSchema)]
pub struct ThreadSummary {
    /// The other party's phone (the thread key).
    pub phone_digits: String,
    /// The customer with this phone.
    pub customer_id: Option<i32>,
    /// The customer's name, else a supplier's.
    pub customer_name: Option<String>,
    /// Text of the latest message (a photo or file label when it has none).
    pub last_message_text: String,
    /// When the latest message was sent.
    pub last_message_at: DateTime<Utc>,
    pub last_direction: Direction,
    /// Agent id on the latest message.
    pub last_agent: Option<String>,
    /// Messages in the conversation (matching the search, with one).
    pub message_count: i64,
    /// Unread messages (1 when marked unread by hand).
    pub unread_count: i64,
}

/// A page of the inbox.
#[derive(Debug, Serialize, ToSchema)]
pub struct ThreadList {
    /// This page.
    pub threads: Vec<ThreadSummary>,
    /// Threads matching, over every page.
    pub total_count: i64,
    /// Unread threads (ignoring the search when it matches nothing).
    pub unread_count: i64,
    /// Another page follows.
    pub has_more: bool,
}

/// Query of the badge and the desktop alerts.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BadgeQuery {
    /// `all` for the whole company (admins only).
    #[serde(default)]
    #[param(inline)]
    pub scope: SmsScope,
}

/// `GET /v1/sms/{provider}/unread-count`.
#[derive(Debug, Serialize, ToSchema)]
pub struct UnreadCount {
    /// Conversations with unread messages.
    pub count: i64,
}

/// An unread conversation whose latest message is from the customer.
#[derive(Debug, Serialize, ToSchema)]
pub struct DesktopAlert {
    /// `sms-<phone>`.
    pub id: String,
    /// The other party's phone.
    pub phone_digits: String,
    /// The customer's name, else the phone.
    pub title: String,
    /// The latest message.
    pub message: String,
    /// The conversation's page.
    pub href: String,
    /// When the latest message arrived.
    pub sent_at: DateTime<Utc>,
}

/// `GET /v1/sms/{provider}/desktop-alerts`.
#[derive(Debug, Serialize, ToSchema)]
pub struct DesktopAlerts {
    /// Newest first.
    pub alerts: Vec<DesktopAlert>,
}

/// A rep who texts through the provider.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct SalesRep {
    /// Filter value for `agent_id`.
    pub agent_id: String,
    /// The rep's name.
    pub name: String,
}

/// Query of `GET /v1/sms/{provider}/threads/{phone}`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ThreadQuery {
    /// Messages, 1 to 200 (default 30), newest first before reversing.
    pub limit: Option<i64>,
    /// Only messages older than this one (paging back).
    pub before_id: Option<i64>,
    /// `all` to read as an admin (anyone else reads their own).
    #[serde(default)]
    #[param(inline)]
    pub scope: SmsScope,
    /// With `scope=all`, read as one rep (their agent id).
    pub agent_id: Option<String>,
}

/// A file sent with a message.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct Attachment {
    /// The attachment's id.
    pub id: i32,
    #[serde(skip)]
    pub sms_id: i32,
    /// Public URL of the file, to show or download it.
    pub url: String,
    /// MIME type, e.g. `image/jpeg`; `image/*` ones are photos.
    pub content_type: String,
    /// The file's name, `file` when unknown.
    pub filename: String,
    /// Width of a photo in pixels; null when unknown or not a photo.
    pub width: Option<i32>,
    /// Height of a photo in pixels; null when unknown or not a photo.
    pub height: Option<i32>,
}

/// Delivery state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum MessageStatus {
    Sent,
    Sending,
    Failed,
}

/// One message.
#[derive(Debug, Serialize, ToSchema)]
pub struct Message {
    /// The SMS row id (page back with `before_id`).
    pub id: i32,
    pub direction: Direction,
    /// The text; empty for a photo or file alone.
    pub text: String,
    /// Agent id stored on the row.
    pub agent: Option<String>,
    /// The rep who sent it, or whose number received it.
    pub agent_id: Option<String>,
    /// That rep's name.
    pub agent_name: Option<String>,
    /// When it was sent (`CloudTalk`: placed among its neighbours when the
    /// webhook arrived late).
    pub created_at: DateTime<Utc>,
    pub status: MessageStatus,
    /// Photos and files, in order.
    pub attachments: Vec<Attachment>,
}

/// The customer a phone belongs to.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct ThreadCustomer {
    pub id: i32,
    pub name: Option<String>,
    pub sales_rep_name: Option<String>,
}

/// An SMS flow running (or failed) on the phone.
#[derive(Debug, Serialize, ToSchema)]
pub struct ActiveFlow {
    /// The phone's enrollment in the flow.
    pub enrollment_id: i32,
    /// The flow.
    pub flow_id: i32,
    /// The flow's name.
    pub flow_name: String,
    /// `active`, `paused` or `failed`.
    pub status: String,
    /// Position of the next step (1 = first).
    pub next_step_position: i32,
    /// The next step's template.
    pub next_step_name: Option<String>,
    /// What the next step sends.
    pub next_step_preview: Option<String>,
    /// When the next step goes out.
    pub next_send_at: Option<DateTime<Utc>>,
    /// Steps in the flow.
    pub total_steps: i64,
    /// Why the flow failed.
    pub error_message: Option<String>,
}

/// `GET /v1/sms/{provider}/threads/{phone}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ThreadDetail {
    pub phone_digits: String,
    pub customer: Option<ThreadCustomer>,
    /// A supplier with this phone, when no customer has it.
    pub supplier_name: Option<String>,
    /// Oldest first.
    pub messages: Vec<Message>,
    /// Unread messages; 0 in the caller's own inbox, which marks it read.
    pub unread_count: i64,
    /// The caller can text through this provider.
    pub can_send: bool,
    /// Older messages exist (page with `before_id`).
    pub has_older: bool,
    pub active_flow: Option<ActiveFlow>,
}
