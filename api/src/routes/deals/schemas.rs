use crate::serde_helpers::{double_option, js_date};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::{IntoParams, ToSchema};

/// Where a deal is in its life: still being worked (`open`), `won` or `lost`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DealStatus {
    /// In the pipeline (`is_won` is null).
    #[default]
    Open,
    /// Closed won.
    Won,
    /// Closed lost.
    Lost,
}

impl DealStatus {
    /// SQL condition on `<alias>.is_won` for this status.
    pub fn sql(self, alias: &str) -> String {
        match self {
            Self::Open => format!("{alias}.is_won IS NULL"),
            Self::Won => format!("{alias}.is_won = 1"),
            Self::Lost => format!("{alias}.is_won = 0"),
        }
    }

    pub const fn from_is_won(is_won: Option<i32>) -> Self {
        match is_won {
            None => Self::Open,
            Some(1) => Self::Won,
            Some(_) => Self::Lost,
        }
    }
}

/// Query of `GET /v1/deal-groups`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GroupsQuery {
    /// Which of the caller's deals `has_overdue_activity` looks at. Default `open`.
    pub status: Option<DealStatus>,
}

/// A pipeline (a set of stages) on the deals board.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct DealGroup {
    /// Group id; pass it as `group_id` to `GET /v1/deals` and `GET /v1/deal-lists`.
    pub id: i32,
    /// Display name.
    pub name: String,
    /// One of the caller's deals in this group (with the requested status)
    /// has an unfinished activity whose deadline day has passed.
    pub has_overdue_activity: bool,
}

/// Query of `GET /v1/deal-lists`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListsQuery {
    /// Only the stages of this group, in board order. Without it: every stage
    /// of every displayed group, ordered by group name then position.
    pub group_id: Option<i32>,
}

/// A stage (board column) of a group.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct DealList {
    /// Stage id (`list_id` of a deal).
    pub id: i32,
    /// Column title.
    pub name: String,
    /// Order inside the group, ascending.
    pub position: i32,
    /// Group the stage belongs to.
    pub group_id: i32,
    /// Name of that group.
    pub group_name: String,
}

/// Query of `GET /v1/deals`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BoardQuery {
    /// Group whose stages make the board (see `GET /v1/deal-groups`).
    pub group_id: i32,
    /// Default `open`.
    pub status: Option<DealStatus>,
}

/// The customer a deal is for, as the board card shows it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DealCustomer {
    /// Customer id.
    pub id: i32,
    /// Contact name.
    pub name: Option<String>,
    /// Company of the contact, when it is a business.
    pub company_name: Option<String>,
    /// Lead temperature: `hot`, `medium`, `cold`, or null when unrated.
    pub temperature: Option<String>,
}

/// The unfinished activity due first on a deal.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NextActivity {
    /// Activity id.
    pub id: i32,
    /// What to do.
    pub name: String,
    /// Store wall time `YYYY-MM-DDTHH:MM:SS` (midnight means a whole day), or null.
    pub deadline: Option<String>,
    /// `low`, `medium` or `high`.
    pub priority: String,
}

/// A deal card of the board.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BoardDeal {
    /// Deal id.
    pub id: i64,
    /// Short name of the job.
    pub title: Option<String>,
    /// Expected value in dollars, a decimal string (`"2500.50"`), or null.
    pub amount: Option<String>,
    /// Stage the deal is in.
    pub list_id: i32,
    /// Order inside the stage, ascending.
    pub position: i32,
    /// Stage name when the deal last moved (legacy label).
    pub status: Option<String>,
    /// `open`, `won` or `lost`.
    pub outcome: DealStatus,
    /// Why a lost deal was lost.
    pub lost_reason: Option<String>,
    /// Expected close date `YYYY-MM-DD`, or null.
    pub due_date: Option<String>,
    /// The customer; null when the customer was deleted.
    pub customer: Option<DealCustomer>,
    /// Photos were attached to the deal.
    pub has_images: bool,
    /// Emails were exchanged on the deal or with its customer.
    pub has_emails: bool,
    /// The deal has notes.
    pub has_notes: bool,
    /// Unfinished activities.
    pub open_activity_count: i64,
    /// Earliest deadline among the unfinished activities (store wall time), or null.
    pub earliest_activity_deadline: Option<String>,
    /// The unfinished activity due first (activities without a deadline come last).
    pub next_activity: Option<NextActivity>,
    /// Phone digits of an active or paused SMS follow-up flow for the customer.
    pub sms_flow_phone: Option<String>,
}

/// One deal in full.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct DealDetail {
    /// Deal id.
    pub id: i64,
    /// Short name of the job.
    pub title: Option<String>,
    /// Expected value in dollars, a decimal string, or null.
    pub amount: Option<String>,
    /// Current stage.
    pub list_id: i32,
    /// Group of the current stage (the deal's pipeline).
    pub group_id: i32,
    /// Order inside the stage.
    pub position: i32,
    /// `open`, `won` or `lost`.
    #[sqlx(skip)]
    pub outcome: DealStatus,
    #[serde(skip)]
    pub is_won: Option<i32>,
    /// Why a lost deal was lost.
    pub lost_reason: Option<String>,
    /// Expected close date `YYYY-MM-DD`.
    pub due_date: Option<String>,
    /// Owner (a user id).
    pub user_id: Option<i32>,
    /// Customer id.
    pub customer_id: i32,
    /// Customer name.
    pub customer_name: Option<String>,
    /// Live deals of the same customer, this one included.
    pub customer_deal_count: i64,
    /// Photos plus documents attached to the deal.
    pub files_count: i64,
    /// When the deal was created.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
}

/// A stay of a deal in a stage.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct StageVisit {
    /// Stage id.
    pub list_id: i32,
    /// When the deal entered the stage.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub entered_at: Option<DateTime<Utc>>,
    /// When it left; null for the current stage.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub exited_at: Option<DateTime<Utc>>,
}

/// Body of `POST /v1/deals`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DealCreate {
    /// Customer the deal is for.
    pub customer_id: i32,
    /// Stage to put it in.
    pub list_id: i32,
    /// Short name of the job.
    pub title: Option<String>,
    /// Expected value in dollars.
    pub amount: Option<f64>,
    /// Order inside the stage. Default 1.
    pub position: Option<i32>,
    /// Create it already closed: `won` or `lost`. Default `open`.
    pub outcome: Option<DealStatus>,
}

/// Result of `POST /v1/deals`.
#[derive(Debug, Serialize, ToSchema)]
pub struct DealCreated {
    /// The new deal, or the customer's open deal that was updated instead.
    pub id: i64,
    /// False when an open deal of the customer was reused.
    pub created: bool,
}

/// Body of `PATCH /v1/deals/{id}`; only the fields sent change.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct DealPatch {
    /// New title; null clears it.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub title: Option<Option<String>>,
    /// New amount in dollars; null clears it.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<f64>)]
    pub amount: Option<Option<f64>>,
    /// Expected close date `YYYY-MM-DD`; null clears it.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub due_date: Option<Option<String>>,
    /// Move to this stage. A closed deal moved to another stage is reopened.
    pub list_id: Option<i32>,
    /// Order inside the stage.
    pub position: Option<i32>,
    /// Give the deal to another customer of the company.
    pub customer_id: Option<i32>,
}

/// One card move of `POST /v1/deals/reorder`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ReorderItem {
    /// Deal id.
    pub id: i64,
    /// Stage it is now in.
    pub list_id: i32,
    /// Order inside that stage.
    pub position: i32,
}

/// Body of `POST /v1/deals/reorder`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ReorderBody {
    /// Every card whose stage or position changed.
    pub updates: Vec<ReorderItem>,
}

/// Body of `PUT /v1/deals/{id}/outcome`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct OutcomeBody {
    /// `won`, `lost`, or `open` to reopen a closed deal.
    pub outcome: DealStatus,
    /// Why it was lost; only with `lost`.
    pub lost_reason: Option<String>,
}

/// Activity priority.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    #[default]
    Medium,
    High,
}

impl Priority {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// A to-do on a deal.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct Activity {
    /// Activity id.
    pub id: i32,
    /// Deal it belongs to.
    pub deal_id: i64,
    /// What to do.
    pub name: String,
    /// Store wall time `YYYY-MM-DDTHH:MM:SS` (midnight means a whole day), or null.
    pub deadline: Option<String>,
    /// `low`, `medium` or `high`.
    pub priority: String,
    /// Done.
    pub is_completed: bool,
    /// When it was marked done.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub completed_at: Option<DateTime<Utc>>,
    /// When it was created.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
    /// Name of the user who created it.
    pub created_by: Option<String>,
}

/// Body of `POST /v1/deals/{id}/activities`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ActivityInput {
    /// What to do, at most 255 characters.
    pub name: String,
    /// `YYYY-MM-DD` (whole day), or a store wall time
    /// `YYYY-MM-DDTHH:MM:SS` / `YYYY-MM-DD HH:MM:SS`. A time sends the deal
    /// owner a reminder then.
    pub deadline: Option<String>,
    /// Default `medium`.
    pub priority: Option<Priority>,
}

/// Body of `PATCH /v1/deals/{id}/activities/{activity_id}`.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct ActivityPatch {
    /// New name.
    pub name: Option<String>,
    /// New deadline (same formats as when creating); null clears it.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub deadline: Option<Option<String>>,
    /// New priority.
    pub priority: Option<Priority>,
    /// Mark done or not done.
    pub is_completed: Option<bool>,
}

/// An activity name the caller used before.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ActivitySuggestion {
    /// The name, trimmed.
    pub name: String,
    /// How many live activities the caller created with it.
    pub uses: i64,
}

/// A comment under a note.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct NoteComment {
    /// Comment id.
    pub id: i64,
    /// Note it belongs to.
    pub note_id: i64,
    /// Text.
    pub content: String,
    /// When it was written.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
    /// Author name; null for comments by non-admins.
    pub created_by: Option<String>,
}

/// A note on a deal with its comments, oldest comment first.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct Note {
    /// Note id.
    pub id: i64,
    /// Deal it belongs to.
    pub deal_id: i64,
    /// Text.
    pub content: String,
    /// Pinned to the top.
    pub is_pinned: bool,
    /// When it was written.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
    /// Author name.
    pub created_by: Option<String>,
    /// Comments, oldest first.
    #[sqlx(skip)]
    pub comments: Vec<NoteComment>,
}

/// Body creating a note or a comment.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ContentInput {
    /// Text (notes up to 5000 characters, comments up to 2000).
    pub content: String,
}

/// Body of `PATCH /v1/deals/{id}/notes/{note_id}`.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct NotePatch {
    /// New text, up to 5000 characters.
    pub content: Option<String>,
    /// Pin or unpin.
    pub is_pinned: Option<bool>,
}

/// Query of `GET /v1/deals/by-customer`: one of the three is required.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ByCustomerQuery {
    /// Customer id; wins over the other two.
    pub customer_id: Option<i32>,
    /// Any email address of the customer (case-insensitive).
    pub customer_email: Option<String>,
    /// Exact customer name (case-insensitive).
    pub customer_name: Option<String>,
}

/// A note preview on a customer's deal.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct NotePreview {
    /// Note id.
    pub id: i64,
    #[serde(skip)]
    pub deal_id: i64,
    /// Text.
    pub content: String,
    /// When it was written.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
}

/// An activity preview on a customer's deal.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ActivityPreview {
    /// Activity id.
    pub id: i32,
    #[serde(skip)]
    pub deal_id: i64,
    /// What to do.
    pub name: String,
    /// Store wall time, or null.
    pub deadline: Option<String>,
    /// `low`, `medium` or `high`.
    pub priority: String,
    /// Done.
    pub is_completed: bool,
}

/// A deal of a customer, with a preview of its notes and activities.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct CustomerDeal {
    /// Deal id.
    pub id: i64,
    /// Short name of the job.
    pub title: Option<String>,
    /// Stage name when the deal last moved.
    pub status: Option<String>,
    /// Expected value, a decimal string.
    pub amount: Option<String>,
    /// Name of the deal's current stage.
    pub list_name: Option<String>,
    /// Up to two notes, pinned first, then newest.
    #[sqlx(skip)]
    pub notes: Vec<NotePreview>,
    /// Up to two activities, unfinished first, then by deadline.
    #[sqlx(skip)]
    pub activities: Vec<ActivityPreview>,
}

/// Query of `GET /v1/deal-lists/{id}/recipients`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RecipientsQuery {
    /// Also customers whose deal in the stage is won.
    #[serde(default)]
    pub include_won: bool,
    /// Also customers whose deal in the stage is lost.
    #[serde(default)]
    pub include_lost: bool,
}

/// An email address to send a bulk email to.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Recipient {
    /// Lowercased, trimmed address.
    pub email: String,
    /// Customer name, or null.
    pub name: Option<String>,
}
