//! `GET /v1/customers/lead-inbox`: the phone app's lead notifications (Deals
//! › ⋮ › Notifications).
//!
//! A sales rep sees the leads assigned to them in the last 14 days. A sales
//! manager (or an admin) also sees the company's leads of the last 14 days
//! that have no rep yet, with the sales reps and this month's lead count of
//! each, as the Telegram lead alert's buttons show them; assigning goes
//! through `PUT /v1/customers/{id}/sales-rep`. The managers see every lead
//! assigned in the window, not only their own.
//!
//! Everyone, a sales rep too, gets `team`: how many leads each of the
//! company's sales reps got this month, for the screen a lead push opens.

use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::MySqlPool;

/// `positions.id` of a sales rep and of a sales manager.
const SALES_REP: i32 = 1;
const SALES_MANAGER: i32 = 2;
/// How far back the inbox goes.
const DAYS: i32 = 14;
/// The most leads of each list.
const LIMIT: i64 = 100;
/// The onboarding sample customer is never a lead here.
const SAMPLE_CUSTOMER_ID: i32 = 990_000_021;
const SAMPLE_CUSTOMER_NAME: &str = "Onboarding Sample Customer";

/// A lead of the inbox.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct InboxLead {
    pub customer_id: i32,
    pub name: Option<String>,
    pub phone: Option<String>,
    /// When the lead came in.
    pub created_at: Option<DateTime<Utc>>,
    /// When it was first given a rep (null while it has none).
    pub assigned_at: Option<DateTime<Utc>>,
    pub sales_rep_id: Option<i32>,
    pub sales_rep_name: Option<String>,
    /// Its newest live deal, the one the app opens.
    pub deal_id: Option<i64>,
}

/// A sales rep of the company and their lead count this month.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct InboxRep {
    /// The sales rep's user id (`PUT /v1/customers/{id}/sales-rep` takes it).
    pub id: i32,
    /// Their name (null when the account has none).
    pub name: Option<String>,
    /// Leads given to them this month (the Telegram button's count).
    pub month_leads: i64,
}

/// `GET /v1/customers/lead-inbox`.
#[derive(Debug, Serialize, ToSchema)]
pub struct LeadInbox {
    /// A sales manager or an admin: `unassigned` and `reps` are filled in.
    pub is_manager: bool,
    /// Leads of the last 14 days without a rep, newest first (managers).
    pub unassigned: Vec<InboxLead>,
    /// Leads given a rep in the last 14 days, newest first: mine, or the
    /// company's for a manager.
    pub assigned: Vec<InboxLead>,
    /// The company's sales reps to assign a lead to (managers only; empty
    /// for a sales rep).
    pub reps: Vec<InboxRep>,
    /// Every sales rep of the company with the leads given to them this
    /// month (since the 1st, server time), sorted by name. Filled in for
    /// everyone, a sales rep too, so they can compare their count with the
    /// team's; the same numbers as `reps`.
    pub team: Vec<InboxRep>,
}

/// List my lead notifications
///
/// The leads assigned to me in the last 14 days; for a sales manager or an
/// admin, also the company's leads of the last 14 days that have no rep yet
/// and the sales reps to give them to (with this month's lead count each).
/// Everyone also gets `team`: each sales rep of the company and the leads
/// given to them this month.
#[crate::openapi::path(
    get,
    path = "/v1/customers/lead-inbox",
    operation_id = "lead_inbox",
    tag = "customers",
    responses(
        (status = 200, description = "My lead notifications", body = LeadInbox),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn lead_inbox(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<LeadInbox>> {
    let pool = &state.pool;
    let is_manager = user.is_admin || user.is_superuser || {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM users_positions
              WHERE user_id = ? AND company_id = ? AND position_id = ? LIMIT 1",
        )
        .bind(user.id)
        .bind(user.company_id)
        .bind(SALES_MANAGER)
        .fetch_optional(pool)
        .await?;
        found.is_some()
    };
    let assigned = if is_manager {
        leads(pool, user.company_id, Some(None), "c.assigned_date").await?
    } else {
        leads(
            pool,
            user.company_id,
            Some(Some(user.id)),
            "c.assigned_date",
        )
        .await?
    };
    let team = reps(pool, user.company_id).await?;
    let (unassigned, reps) = if is_manager {
        (
            leads(pool, user.company_id, None, "c.created_date").await?,
            team.clone(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    Ok(Json(LeadInbox {
        is_manager,
        unassigned,
        assigned,
        reps,
        team,
    }))
}

/// Live, valid leads of the window by `date`: `rep` None = without a rep,
/// Some(None) = with any rep, Some(Some(id)) = with that rep.
async fn leads(
    pool: &MySqlPool,
    company_id: i32,
    rep: Option<Option<i32>>,
    date: &'static str,
) -> sqlx::Result<Vec<InboxLead>> {
    let who = match rep {
        None => " AND c.sales_rep IS NULL",
        Some(None) => " AND c.sales_rep IS NOT NULL",
        Some(Some(_)) => " AND c.sales_rep = ?",
    };
    let sql = format!(
        "SELECT c.id AS customer_id, c.name, c.phone, c.created_date AS created_at,
                c.assigned_date AS assigned_at, c.sales_rep AS sales_rep_id,
                u.name AS sales_rep_name,
                -- deals.id is BIGINT UNSIGNED, which decodes only as u64.
                (SELECT CAST(d.id AS SIGNED) FROM deals d
                  WHERE d.customer_id = c.id AND d.deleted_at IS NULL
                  ORDER BY d.id DESC LIMIT 1) AS deal_id
           FROM customers c
           LEFT JOIN users u ON u.id = c.sales_rep
          WHERE c.company_id = ? AND c.source = 'leads' AND c.deleted_at IS NULL
            AND (c.invalid_lead IS NULL OR c.invalid_lead = '')
            AND c.id != ? AND (c.name IS NULL OR c.name != ?)
            AND {date} >= NOW() - INTERVAL ? DAY{who}
          ORDER BY {date} DESC, c.id DESC
          LIMIT ?"
    );
    let mut query = sqlx::query_as::<_, InboxLead>(sqlx::AssertSqlSafe(sql))
        .bind(company_id)
        .bind(SAMPLE_CUSTOMER_ID)
        .bind(SAMPLE_CUSTOMER_NAME)
        .bind(DAYS);
    if let Some(Some(id)) = rep {
        query = query.bind(id);
    }
    query.bind(LIMIT).fetch_all(pool).await
}

/// The company's sales reps with this month's leads, as the Telegram lead
/// alert counts them.
async fn reps(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<InboxRep>> {
    sqlx::query_as(
        "SELECT u.id, u.name, COUNT(c.id) AS month_leads
           FROM users u
           JOIN users_positions up ON up.user_id = u.id AND up.company_id = u.company_id
           LEFT JOIN customers c ON c.sales_rep = u.id AND c.source = 'leads'
                AND c.assigned_date >= DATE_FORMAT(NOW(), '%Y-%m-01')
                AND c.company_id = u.company_id AND c.deleted_at IS NULL
          WHERE u.company_id = ? AND u.is_deleted = 0 AND up.position_id = ?
          GROUP BY u.id, u.name
          ORDER BY u.name, u.id",
    )
    .bind(company_id)
    .bind(SALES_REP)
    .fetch_all(pool)
    .await
}
