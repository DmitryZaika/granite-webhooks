//! A customer's history for the customer dialog: who created and assigned
//! them, their deals and what happened to them, reassignments, the email
//! history and the activities and notes of their deals.
//!
//! Replaces `loadCustomerInfoDialog` (`app/utils/customerInfoDialog.server.ts`).

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::routes::emails::deal_history::{
    HistoryEmail, address_emails, history_sql, merge, record_missing_sends, with_thread_attachments,
};
use crate::serde_helpers::js_date;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{AssertSqlSafe, FromRow, MySqlPool};
use utoipa::ToSchema;

/// `customers_history.reassigned_to` of a deal deletion: `deal_deleted:<id>`.
const DEAL_DELETED_PREFIX: &str = "deal_deleted:";

/// Everything the customer dialog shows besides the contact details
/// (`GET /v1/customers/{id}`).
#[derive(Debug, Serialize, ToSchema)]
pub struct CustomerHistory {
    /// Who created the customer (name), if recorded.
    pub created_by: Option<String>,
    /// When the customer was first assigned to a sales rep.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub assigned_date: Option<DateTime<Utc>>,
    /// When the customer was deleted; null when live.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub deleted_at: Option<DateTime<Utc>>,
    /// Parent customer (a project of another customer).
    pub parent_id: Option<i32>,
    /// When the first live deal was created (the lead reached a sales rep).
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub first_rep_deal_created_at: Option<DateTime<Utc>>,
    /// Sales rep of that first deal.
    pub initial_sales_rep_name: Option<String>,
    /// Live deals in live stages, newest first.
    pub deals: Vec<HistoryDeal>,
    /// Reassignments of the customer, oldest first.
    pub reassignments: Vec<Reassignment>,
    /// Every deal ever made for the customer, oldest first.
    pub created_deals: Vec<CreatedDeal>,
    /// Deleted deals.
    pub deleted_deals: Vec<DeletedDeal>,
    /// Won or lost deals.
    pub closed_deals: Vec<ClosedDeal>,
    /// Email history (by address and as a participant), newest first.
    pub emails: Vec<HistoryEmail>,
    /// Activities of the live deals, latest first.
    pub activities: Vec<HistoryActivity>,
    /// Notes of the live deals, pinned first, with comments.
    pub notes: Vec<HistoryNote>,
}

/// A live deal of the customer.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct HistoryDeal {
    /// Deal id.
    pub id: i64,
    /// Amount, a decimal string.
    pub amount: Option<String>,
    /// Why it was lost.
    pub lost_reason: Option<String>,
    /// Stage name.
    pub list_name: String,
    /// When it was created.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
}

/// A reassignment of the customer.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct Reassignment {
    /// Who reassigned.
    pub reassigned_by: Option<String>,
    /// To whom.
    pub reassigned_to: Option<String>,
    /// When.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub updated_at: Option<DateTime<Utc>>,
}

/// A deal created for the customer.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedDeal {
    /// Deal id.
    pub id: i64,
    /// When.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
    /// By whom (name).
    pub created_by: Option<String>,
}

/// A deleted deal.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeletedDeal {
    /// Deal id.
    pub id: i64,
    /// When.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub deleted_at: Option<DateTime<Utc>>,
    /// By whom (name), when recorded.
    pub deleted_by: Option<String>,
}

/// A won or lost deal.
#[derive(Debug, Serialize, ToSchema)]
pub struct ClosedDeal {
    /// Deal id.
    pub id: i64,
    /// `won` or `lost`.
    pub outcome: &'static str,
    /// Why it was lost.
    pub lost_reason: Option<String>,
    /// When it left its last stage.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub closed_at: Option<DateTime<Utc>>,
}

/// An activity of one of the customer's deals.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct HistoryActivity {
    /// Activity id.
    pub id: i64,
    /// Deal.
    pub deal_id: i64,
    /// Company.
    pub company_id: i32,
    /// What to do.
    pub name: String,
    /// Store wall time `YYYY-MM-DDTHH:MM:SS`, or null.
    pub deadline: Option<String>,
    /// `low`, `medium` or `high`.
    pub priority: String,
    /// 1 when done.
    pub is_completed: i32,
    /// When it was done.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub completed_at: Option<DateTime<Utc>>,
    /// When it was created.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
    /// By whom (name).
    pub created_by: Option<String>,
    /// Stage the deal is in.
    pub deal_list_name: Option<String>,
}

/// A note of one of the customer's deals.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct HistoryNote {
    /// Note id.
    pub id: i64,
    /// Deal.
    pub deal_id: i64,
    /// Company.
    pub company_id: i32,
    /// Text.
    pub content: String,
    /// 1 when pinned.
    pub is_pinned: i32,
    /// When.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
    /// By whom (name).
    pub created_by: Option<String>,
    /// Comments, oldest first.
    #[sqlx(skip)]
    pub comments: Vec<HistoryComment>,
}

/// A comment on a note.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct HistoryComment {
    /// Comment id.
    pub id: i64,
    /// Note.
    pub note_id: i64,
    /// Text.
    pub content: String,
    /// When.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
    /// By whom (name).
    pub created_by: Option<String>,
}

#[derive(FromRow)]
struct LifecycleRow {
    id: i64,
    created_by: Option<String>,
    created_at: Option<DateTime<Utc>>,
    deleted_at: Option<DateTime<Utc>>,
    deleted_by: Option<String>,
    is_won: Option<i32>,
    lost_reason: Option<String>,
    closed_at: Option<DateTime<Utc>>,
}

type CustomerRow = (
    Option<String>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<i32>,
);

/// Get a customer's history
///
/// The customer dialog's history: creation and assignment, deals (live,
/// created, deleted, won or lost), reassignments, email history (first
/// recording drips that were sent but are missing from it) and the
/// activities and notes of the live deals. Deleted customers included.
#[utoipa::path(
    get,
    path = "/v1/customers/{id}/history",
    operation_id = "get_customer_history",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id")),
    responses(
        (status = 200, description = "The history", body = CustomerHistory),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn history(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<CustomerHistory>> {
    let pool = &state.pool;
    let company_id = user.company_id;
    let customer: Option<CustomerRow> = sqlx::query_as(
        "SELECT created_by, assigned_date, deleted_at, parent_id
         FROM customers WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let (created_by, assigned_date, deleted_at, parent_id) = customer.ok_or(ApiError::NotFound)?;

    let deals: Vec<HistoryDeal> = sqlx::query_as(
        "SELECT CAST(d.id AS SIGNED) AS id, CAST(d.amount AS CHAR) AS amount, d.lost_reason,
                l.name AS list_name, d.created_at
         FROM deals d JOIN deals_list l ON l.id = d.list_id
         WHERE d.customer_id = ? AND d.deleted_at IS NULL AND l.deleted_at IS NULL
         ORDER BY d.id DESC",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let reassignments: Vec<Reassignment> = sqlx::query_as(
        "SELECT reassigned_by, reassigned_to, updated_at FROM customers_history
         WHERE customer_id = ? AND (reassigned_to IS NULL OR reassigned_to NOT LIKE ?)
         ORDER BY updated_at, id",
    )
    .bind(id)
    .bind(format!("{DEAL_DELETED_PREFIX}%"))
    .fetch_all(pool)
    .await?;
    let lifecycle = lifecycle(pool, id).await?;
    let first: Option<(Option<DateTime<Utc>>, Option<String>)> = sqlx::query_as(
        "SELECT d.created_at, u.name FROM deals d
         LEFT JOIN users u ON u.id = d.user_id AND u.is_deleted = 0
         WHERE d.customer_id = ? AND d.deleted_at IS NULL
         ORDER BY d.created_at, d.id LIMIT 1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let (first_rep_deal_created_at, initial_sales_rep_name) = first.unwrap_or_default();

    record_missing_sends(pool, None, id).await;
    let emails = customer_emails(pool, company_id, id).await?;
    let (activities, notes) = if deleted_at.is_none() {
        (
            activities(pool, company_id, id).await?,
            notes(pool, company_id, id).await?,
        )
    } else {
        (Vec::new(), Vec::new())
    };

    let mut created_deals = Vec::new();
    let mut deleted_deals = Vec::new();
    let mut closed_deals = Vec::new();
    for row in lifecycle {
        created_deals.push(CreatedDeal {
            id: row.id,
            created_at: row.created_at,
            created_by: row.created_by,
        });
        if row.deleted_at.is_some() {
            deleted_deals.push(DeletedDeal {
                id: row.id,
                deleted_at: row.deleted_at,
                deleted_by: row.deleted_by,
            });
        }
        let outcome = match row.is_won {
            Some(1) => Some("won"),
            Some(0) => Some("lost"),
            _ => None,
        };
        if let Some(outcome) = outcome {
            closed_deals.push(ClosedDeal {
                id: row.id,
                outcome,
                lost_reason: row.lost_reason,
                closed_at: row.closed_at,
            });
        }
    }

    Ok(Json(CustomerHistory {
        created_by,
        assigned_date,
        deleted_at,
        parent_id,
        first_rep_deal_created_at,
        initial_sales_rep_name,
        deals,
        reassignments,
        created_deals,
        deleted_deals,
        closed_deals,
        emails,
        activities,
        notes,
    }))
}

/// Every deal of the customer with who created and deleted it and how it closed.
async fn lifecycle(pool: &MySqlPool, customer_id: i32) -> Result<Vec<LifecycleRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(d.id AS SIGNED) AS id, NULLIF(TRIM(d.created_by), '') AS created_by,
                d.created_at, d.deleted_at,
                NULLIF(TRIM(h.reassigned_by), '') AS deleted_by,
                d.is_won, NULLIF(TRIM(d.lost_reason), '') AS lost_reason,
                CASE WHEN d.is_won IS NULL THEN NULL ELSE (
                  SELECT dsh.exited_at FROM deal_stage_history dsh
                  WHERE dsh.deal_id = d.id AND dsh.exited_at IS NOT NULL
                  ORDER BY dsh.exited_at DESC, dsh.id DESC LIMIT 1
                ) END AS closed_at
         FROM deals d
         LEFT JOIN customers_history h ON h.id = (
           SELECT h2.id FROM customers_history h2
           WHERE h2.customer_id = d.customer_id AND h2.reassigned_to = CONCAT(?, d.id)
           ORDER BY h2.id DESC LIMIT 1
         )
         WHERE d.customer_id = ?
         ORDER BY d.id",
    )
    .bind(DEAL_DELETED_PREFIX)
    .bind(customer_id)
    .fetch_all(pool)
    .await
}

/// `getCustomerHistoryEmailsWithReads`: mail with the customer's addresses,
/// then the threads the customer took part in.
async fn customer_emails(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> Result<Vec<HistoryEmail>, sqlx::Error> {
    let addresses: Vec<String> = sqlx::query_scalar(
        "SELECT ce.email FROM customers_emails ce JOIN customers c ON c.id = ce.customer_id
         WHERE ce.customer_id = ? ORDER BY ce.id = c.email_id DESC, ce.created_at, ce.id",
    )
    .bind(customer_id)
    .fetch_all(pool)
    .await?;
    let addresses: Vec<String> = addresses
        .into_iter()
        .map(|a| a.trim().to_lowercase())
        .filter(|a| !a.is_empty())
        .collect();
    let by_address = address_emails(pool, company_id, &addresses).await?;
    let by_participant: Vec<HistoryEmail> = history_sql(
        "e.thread_id IN (SELECT DISTINCT e2.thread_id FROM emails e2
           JOIN email_participants ep ON ep.email_id = e2.id
           WHERE ep.customer_id = ? AND e2.thread_id IS NOT NULL AND e2.deleted_at IS NULL)",
        vec![company_id.into(), customer_id.into()],
    )
    .query_as()
    .fetch_all(pool)
    .await?;
    Ok(with_thread_attachments(merge(by_address, by_participant)))
}

async fn activities(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> Result<Vec<HistoryActivity>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(da.id AS SIGNED) AS id, CAST(da.deal_id AS SIGNED) AS deal_id, da.company_id, da.name,
                DATE_FORMAT(da.deadline, '%Y-%m-%dT%H:%i:%s') AS deadline,
                CAST(da.priority AS CHAR) AS priority, CAST(da.is_completed AS SIGNED) AS is_completed,
                da.completed_at, da.created_at, da.created_by, dl.name AS deal_list_name
         FROM deal_activities da
         JOIN deals d ON d.id = da.deal_id AND d.deleted_at IS NULL
         JOIN customers c ON c.id = d.customer_id AND c.company_id = ? AND c.deleted_at IS NULL
         LEFT JOIN deals_list dl ON dl.id = d.list_id AND dl.deleted_at IS NULL
         WHERE d.customer_id = ? AND da.company_id = ? AND da.deleted_at IS NULL
         ORDER BY COALESCE(da.completed_at, da.created_at) DESC, da.id DESC",
    )
    .bind(company_id)
    .bind(customer_id)
    .bind(company_id)
    .fetch_all(pool)
    .await
}

async fn notes(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> Result<Vec<HistoryNote>, sqlx::Error> {
    let mut notes: Vec<HistoryNote> = sqlx::query_as(
        "SELECT CAST(dn.id AS SIGNED) AS id, CAST(dn.deal_id AS SIGNED) AS deal_id, dn.company_id, dn.content,
                CAST(dn.is_pinned AS SIGNED) AS is_pinned, dn.created_at, dn.created_by
         FROM deal_notes dn
         JOIN deals d ON d.id = dn.deal_id AND d.deleted_at IS NULL
         JOIN customers c ON c.id = d.customer_id AND c.company_id = ? AND c.deleted_at IS NULL
         WHERE d.customer_id = ? AND dn.company_id = ? AND dn.deleted_at IS NULL
         ORDER BY dn.is_pinned DESC, dn.created_at DESC, dn.id DESC",
    )
    .bind(company_id)
    .bind(customer_id)
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    if notes.is_empty() {
        return Ok(notes);
    }
    let marks = vec!["?"; notes.len()].join(",");
    let mut query = sqlx::query_as::<_, HistoryComment>(AssertSqlSafe(format!(
        "SELECT CAST(id AS SIGNED) AS id, CAST(note_id AS SIGNED) AS note_id, content, created_at, created_by FROM deal_note_comments
         WHERE note_id IN ({marks}) AND company_id = ? AND deleted_at IS NULL
         ORDER BY created_at, id"
    )));
    for note in &notes {
        query = query.bind(note.id);
    }
    let comments = query.bind(company_id).fetch_all(pool).await?;
    for comment in comments {
        if let Some(note) = notes.iter_mut().find(|note| note.id == comment.note_id) {
            note.comments.push(comment);
        }
    }
    Ok(notes)
}
