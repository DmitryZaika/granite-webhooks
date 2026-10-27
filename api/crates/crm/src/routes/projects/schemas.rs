use super::access::{Permissions, Stage};
use crate::openapi::{IntoParams, ToSchema};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// A project, with its transaction's milestones. Dates are UTC ISO strings.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct Project {
    pub id: i32,
    /// The won deal it came from.
    pub source_deal_id: i64,
    pub customer_id: i32,
    /// Its transaction, once linked.
    pub sale_id: Option<i32>,
    pub sales_rep_id: Option<i32>,
    pub owner_id: Option<i32>,
    pub title: String,
    pub quoted_amount: Option<f64>,
    pub project_address: Option<String>,
    /// The stage, Installed and Paid & Completed read from the transaction.
    #[sqlx(try_from = "String")]
    pub stage_code: Stage,
    pub position: i32,
    pub is_on_hold: bool,
    pub cancelled_at: Option<String>,
    pub templated_at: Option<String>,
    pub archived_at: Option<String>,
    /// Bumped on every change; actions send it back to catch a stale page.
    pub version: i32,
    pub created_at: String,
    pub updated_at: String,
    pub installed_at: Option<String>,
    pub paid_at: Option<String>,
    /// The transaction was cancelled (the project counts as cancelled).
    pub sale_cancelled_at: Option<String>,
    pub transaction_price: Option<f64>,
    pub customer_name: String,
    pub owner_name: Option<String>,
    pub sales_rep_name: Option<String>,
    /// Every active slab of the transaction is cut.
    pub ready_to_install: bool,
}

/// Someone in the company.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct Person {
    pub id: i32,
    pub name: String,
}

/// A task on a project, for anyone in the company.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ProjectTask {
    pub id: i32,
    pub project_id: i32,
    pub name: String,
    pub assigned_user_id: Option<i32>,
    pub assigned_user_name: Option<String>,
    /// Store wall time `YYYY-MM-DDTHH:MM:SS`; `T00:00:00` is a date with no time.
    pub deadline: Option<String>,
    pub priority: String,
    pub completed_at: Option<String>,
    pub created_by: i32,
    pub created_by_name: Option<String>,
    pub created_at: String,
}

/// The next template or installation appointment.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ProjectAppointment {
    pub id: i32,
    pub project_id: i32,
    /// `templates` or `installations` (the calendar).
    pub kind: String,
    pub title: String,
    pub start_date: String,
    /// Waiting for a date on the schedule (`start_date` means nothing yet).
    pub unscheduled: bool,
}

/// Query of `GET /v1/projects`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BoardQuery {
    /// `active` (default), `completed` or `cancelled`.
    pub view: Option<String>,
    /// An owner's user id, or `unassigned`.
    pub owner: Option<String>,
    /// Title, customer or address contains this.
    pub q: Option<String>,
    /// Only projects on hold.
    #[serde(default)]
    pub hold: bool,
    /// Include archived projects.
    #[serde(default)]
    pub archived: bool,
    /// 1-based page of 100.
    pub page: Option<u32>,
}

/// `GET /v1/projects`: the caller's board.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectsBoard {
    pub projects: Vec<Project>,
    /// The columns the caller has, in order.
    pub stages: Vec<Stage>,
    pub owners: Vec<Person>,
    /// The next appointments of these projects.
    pub appointments: Vec<ProjectAppointment>,
    /// Their open tasks, the most urgent first.
    pub tasks: Vec<ProjectTask>,
    pub permissions: Permissions,
    pub page: u32,
    pub has_next: bool,
}

#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ProjectNote {
    pub id: i32,
    pub content: String,
    pub created_by: i32,
    pub author: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ProjectFile {
    pub id: i32,
    pub name: String,
    /// A URL the browser can open.
    pub url: String,
    pub content_type: String,
    pub created_by: i32,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct StageChange {
    pub id: i32,
    pub from_stage: Option<String>,
    pub to_stage: String,
    pub source_action: String,
    pub reason: Option<String>,
    pub author: Option<String>,
    pub created_at: String,
}

/// A transaction of the customer the project may link to.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct LinkableSale {
    pub id: i32,
    pub price: Option<f64>,
    pub project_address: Option<String>,
    pub installed_date: Option<String>,
    pub paid_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct Calendar {
    pub id: i32,
    pub slug: String,
    pub name: String,
}

/// `GET /v1/projects/{id}`: a project's page.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectWorkspace {
    pub project: Project,
    pub tasks: Vec<ProjectTask>,
    pub notes: Vec<ProjectNote>,
    pub files: Vec<ProjectFile>,
    pub history: Vec<StageChange>,
    /// Transactions to link (empty once one is linked).
    pub sales: Vec<LinkableSale>,
    pub people: Vec<Person>,
    /// The Templates and Installations calendars.
    pub calendars: Vec<Calendar>,
    /// Every appointment of the project, newest first.
    pub appointments: Vec<ProjectAppointment>,
    pub permissions: Permissions,
    /// The caller's user id (to show which tasks and notes are theirs).
    pub user_id: i32,
}

/// Body of `POST /v1/projects/{id}/actions`. `version` is the project's as
/// the page has it: a stale page gets a 409.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(tag = "intent", rename_all = "kebab-case")]
pub enum ProjectAction {
    /// Move forward (production).
    Stage {
        version: i32,
        stage: Stage,
    },
    /// Link the customer's transaction.
    LinkSale {
        version: i32,
        #[serde(rename = "saleId")]
        sale_id: i32,
    },
    /// Confirm full payment on this date (ISO).
    Paid {
        version: i32,
        date: String,
    },
    /// Pick the owner (null: nobody).
    Assign {
        version: i32,
        #[serde(rename = "ownerId")]
        owner_id: Option<i32>,
    },
    Hold {
        version: i32,
        #[serde(rename = "onHold")]
        on_hold: bool,
    },
    Cancel {
        version: i32,
        reason: String,
    },
    Archive {
        version: i32,
        archived: bool,
    },
    /// Change or clear the payment date, with a reason.
    CorrectPayment {
        version: i32,
        date: Option<String>,
        reason: String,
    },
    /// Move back, with a reason (managers).
    Correct {
        version: i32,
        stage: Stage,
        reason: String,
    },
}

impl ProjectAction {
    pub const fn version(&self) -> i32 {
        match self {
            Self::Stage { version, .. }
            | Self::LinkSale { version, .. }
            | Self::Paid { version, .. }
            | Self::Assign { version, .. }
            | Self::Hold { version, .. }
            | Self::Cancel { version, .. }
            | Self::Archive { version, .. }
            | Self::CorrectPayment { version, .. }
            | Self::Correct { version, .. } => *version,
        }
    }
}

/// `GET /v1/deals/{id}/project`.
#[derive(Debug, Serialize, ToSchema)]
pub struct DealProject {
    /// The deal's project, if it has one.
    pub project_id: Option<i32>,
    /// Projects are on for the company and the caller may create one.
    pub can_convert: bool,
}

/// `POST /v1/deals/{id}/project`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConvertedDeal {
    pub project_id: i32,
    /// False when the deal had a project already.
    pub created: bool,
}

/// Body of `POST /v1/projects/{id}/tasks`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TaskInput {
    /// 1 to 255 characters.
    pub name: String,
    /// Who does it: anyone in the company; null for nobody.
    #[serde(rename = "assignedUserId", default)]
    pub assigned_user_id: Option<i32>,
    /// `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM[:SS]` store wall time.
    #[serde(default)]
    pub deadline: Option<String>,
    /// `low`, `medium` (default) or `high`.
    #[serde(default)]
    pub priority: Option<String>,
}

/// Body of `PATCH /v1/projects/{id}/tasks/{task_id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TaskUpdate {
    /// Done (true) or open again (false).
    pub completed: bool,
}

/// Body of `POST /v1/projects/{id}/notes`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NoteInput {
    pub content: String,
}

/// One file uploaded with `POST /v1/uploads` (folder `deals`).
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewProjectFile {
    pub url: String,
    pub name: String,
    #[serde(rename = "contentType", default)]
    pub content_type: Option<String>,
}

/// Body of `POST /v1/projects/{id}/files`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewProjectFiles {
    pub files: Vec<NewProjectFile>,
}
