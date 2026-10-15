//! Transactions: the company's sales as the employee, admin and shop
//! Transactions pages list them.
//!
//! - `GET  /v1/transactions`                 → sales with their slabs' cut state
//! - `GET  /v1/transactions/sales-reps`      → who sold something (the filter)
//! - `POST /v1/transactions/{id}/installed`  → mark a fully cut sale installed
//!
//! Replaces the loaders and the `mark-installed` actions of the Remix
//! `employee.transactions`, `admin.transactions` and `shop.transactions`
//! pages.

use crate::auth::ShopOrEmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::routes::stones::queries::escape_like;
use crate::serde_helpers::js_date;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(sales_reps))
        .routes(routes!(mark_installed))
}

/// Which Transactions page asks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TransactionsView {
    /// Employees (and the shop): `sf` is the sale's square feet.
    #[default]
    Employee,
    /// The shop: `sf` adds up each room's square feet.
    Shop,
    /// Admins: every sale of live sellers.
    Admin,
}

/// Which sales.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SaleProgress {
    /// Every sale.
    #[default]
    All,
    /// Not installed nor cancelled.
    InProgress,
    /// Installed or cancelled.
    Finished,
}

/// Query of `GET /v1/transactions`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TransactionsQuery {
    /// Default `employee`; `admin` needs an admin.
    #[serde(default)]
    #[param(inline)]
    pub view: TransactionsView,
    /// Default `all`.
    #[serde(default)]
    #[param(inline)]
    pub status: SaleProgress,
    /// Only sales of this seller (by name).
    pub sales_rep: Option<String>,
    /// Customer, seller, bundle or stone name containing this.
    pub search: Option<String>,
}

/// A sale in the list.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct TransactionRow {
    /// Sale id.
    pub id: i32,
    /// When it was sold.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub sale_date: Option<DateTime<Utc>>,
    /// Customer name.
    pub customer_name: Option<String>,
    /// Seller name.
    pub seller_name: Option<String>,
    /// Bundles, comma separated.
    pub bundle: Option<String>,
    /// When it was cancelled.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub cancelled_date: Option<DateTime<Utc>>,
    /// When it was installed.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub installed_date: Option<DateTime<Utc>>,
    /// `name:ACTIVE|DELETED` per stone, comma separated.
    pub stone_name: Option<String>,
    /// Project address.
    pub project_address: Option<String>,
    /// Square feet (the shop view: each room's, added up).
    pub sf: Option<f64>,
    /// `bundle:CUT|UNCUT:ACTIVE|DELETED` per slab, comma separated.
    pub bundle_with_cut: Option<String>,
    /// 1 when every slab is cut.
    pub all_cut: Option<i64>,
    /// 1 when a slab is cut.
    pub any_cut: Option<i64>,
    /// Slabs on the sale.
    pub total_slabs: i64,
    /// Of them cut.
    pub cut_slabs: Option<i64>,
    /// Sink models, joined.
    #[sqlx(skip)]
    pub sink_type: Option<String>,
    /// `Sold`, `Partially Cut`, `Cut`, `Installed` or `Cancelled`.
    #[sqlx(skip)]
    pub status: String,
}

fn status_of(row: &TransactionRow) -> &'static str {
    if row.cancelled_date.is_some() {
        "Cancelled"
    } else if row.installed_date.is_some() {
        "Installed"
    } else if row.all_cut == Some(1) {
        "Cut"
    } else if row.any_cut == Some(1) {
        "Partially Cut"
    } else {
        "Sold"
    }
}

/// The shop view's square feet: one value per room (by its id, else name
/// and bundle per slab), added up.
const ROOM_SQUARE_FEET: &str = "(SELECT SUM(room_sqft) FROM (
       SELECT MAX(si2.square_feet) AS room_sqft FROM slab_inventory si2
        WHERE si2.sale_id = s.id
        GROUP BY CASE
          WHEN si2.room_uuid IS NOT NULL THEN CONCAT('uuid:', HEX(si2.room_uuid))
          WHEN COALESCE(NULLIF(TRIM(si2.room), ''), '') <> '' THEN
            CONCAT('room:', LOWER(TRIM(si2.room)), ':bundle:', COALESCE(si2.bundle, ''), ':slab:', si2.id)
          ELSE CONCAT('bundle:', COALESCE(si2.bundle, ''), ':slab:', si2.id)
        END) room_totals)";

/// List transactions
///
/// The company's sales, newest first, with their bundles, stones, sinks,
/// cut progress and a status. `employee` and `shop` views filter by
/// `status`, `sales_rep` and `search`; the `admin` view lists every sale of
/// live sellers.
#[utoipa::path(
    get,
    path = "/v1/transactions",
    operation_id = "list_transactions",
    tag = "sales",
    params(TransactionsQuery),
    responses(
        (status = 200, description = "Sales", body = Vec<TransactionRow>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop, or the admin view without being an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    MultiQuery(query): MultiQuery<TransactionsQuery>,
) -> ApiResult<Json<Vec<TransactionRow>>> {
    let view = query.view;
    if view == TransactionsView::Admin && !(user.is_admin || user.is_superuser) {
        return Err(ApiError::Forbidden);
    }
    let sf = if view == TransactionsView::Shop {
        ROOM_SQUARE_FEET
    } else {
        "CAST(s.square_feet AS DOUBLE)"
    };
    let live_sellers = if view == TransactionsView::Admin {
        " AND u.is_deleted = 0"
    } else {
        ""
    };
    let mut sql = QueryBuilder::<MySql>::new(format!(
        "SELECT s.id, s.sale_date, c.name AS customer_name, u.name AS seller_name,
                GROUP_CONCAT(DISTINCT si.bundle) AS bundle, s.cancelled_date, s.installed_date,
                GROUP_CONCAT(DISTINCT CONCAT(st.name, ':', IF(si.deleted_at IS NOT NULL, 'DELETED', 'ACTIVE'))) AS stone_name,
                s.project_address, CAST({sf} AS DOUBLE) AS sf,
                GROUP_CONCAT(DISTINCT CONCAT(si.bundle, ':', IF(si.cut_date IS NOT NULL, 'CUT', 'UNCUT'), ':',
                                             IF(si.deleted_at IS NOT NULL, 'DELETED', 'ACTIVE'))) AS bundle_with_cut,
                CAST(MIN(CASE WHEN si.cut_date IS NULL THEN 0 ELSE 1 END) AS SIGNED) AS all_cut,
                CAST(MAX(CASE WHEN si.cut_date IS NOT NULL THEN 1 ELSE 0 END) AS SIGNED) AS any_cut,
                COUNT(si.id) AS total_slabs,
                CAST(SUM(CASE WHEN si.cut_date IS NOT NULL THEN 1 ELSE 0 END) AS SIGNED) AS cut_slabs
           FROM sales s
           JOIN customers c ON s.customer_id = c.id
           JOIN users u ON s.seller_id = u.id{live_sellers}
           LEFT JOIN slab_inventory si ON s.id = si.sale_id
           LEFT JOIN stones st ON si.stone_id = st.id
          WHERE s.company_id = "
    ));
    sql.push_bind(user.company_id);
    if view != TransactionsView::Admin {
        if let Some(rep) = query
            .sales_rep
            .as_deref()
            .filter(|r| !r.is_empty() && *r != "All")
        {
            sql.push(" AND u.name = ").push_bind(rep.to_string());
        }
        match query.status {
            SaleProgress::InProgress => {
                sql.push(" AND s.installed_date IS NULL AND s.cancelled_date IS NULL");
            }
            SaleProgress::Finished => {
                sql.push(" AND (s.installed_date IS NOT NULL OR s.cancelled_date IS NOT NULL)");
            }
            SaleProgress::All => {}
        }
        if let Some(search) = query
            .search
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let pattern = format!("%{}%", escape_like(search));
            sql.push(" AND (c.name LIKE ")
                .push_bind(pattern.clone())
                .push(" OR u.name LIKE ")
                .push_bind(pattern.clone())
                .push(" OR si.bundle LIKE ")
                .push_bind(pattern.clone())
                .push(" OR st.name LIKE ")
                .push_bind(pattern)
                .push(")");
        }
    }
    sql.push(" GROUP BY s.id, s.sale_date, c.name, u.name ORDER BY s.sale_date DESC, s.id DESC");
    let mut rows: Vec<TransactionRow> = sql.build_query_as().fetch_all(&state.pool).await?;
    let separator = if view == TransactionsView::Admin {
        ","
    } else {
        ", "
    };
    let sinks: Vec<(i32, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT s.id, GROUP_CONCAT(st.name SEPARATOR '{separator}')
           FROM sales s
           JOIN slab_inventory si ON s.id = si.sale_id
           JOIN sinks sk ON si.id = sk.slab_id
           JOIN sink_type st ON sk.sink_type_id = st.id
          WHERE s.company_id = ?
          GROUP BY s.id"
    )))
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    let mut sinks: HashMap<i32, Option<String>> = sinks.into_iter().collect();
    for row in &mut rows {
        row.sink_type = sinks.remove(&row.id).flatten();
        row.status = status_of(row).to_string();
    }
    Ok(Json(rows))
}

/// List the sellers to filter by
///
/// Names of everyone who sold something in the company.
#[utoipa::path(
    get,
    path = "/v1/transactions/sales-reps",
    operation_id = "list_transaction_sellers",
    tag = "sales",
    responses(
        (status = 200, description = "Seller names", body = Vec<String>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn sales_reps(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
) -> ApiResult<Json<Vec<String>>> {
    let names: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT DISTINCT users.name FROM users JOIN sales ON users.id = sales.seller_id
          WHERE sales.company_id = ? ORDER BY users.name",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(names.into_iter().flatten().collect()))
}

/// Body of `POST /v1/transactions/{id}/installed`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Installed {
    /// When it was installed (`YYYY-MM-DD` or an ISO time); default now.
    #[serde(default)]
    pub installed_date: Option<String>,
    /// It is also paid (on the same date).
    #[serde(default)]
    pub paid: bool,
}

fn installed_at(raw: Option<&str>) -> ApiResult<NaiveDateTime> {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(Utc::now().naive_utc());
    };
    if let Ok(time) = DateTime::parse_from_rfc3339(raw) {
        return Ok(time.with_timezone(&Utc).naive_utc());
    }
    chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map(|date| date.and_hms_opt(0, 0, 0).unwrap_or_default())
        .map_err(|_| ApiError::BadRequest("Invalid installed date".into()))
}

/// Mark a sale installed
///
/// Sets the installed date (and, when `paid`, the paid date) of a sale of
/// the company whose slabs are all cut.
#[utoipa::path(
    post,
    path = "/v1/transactions/{id}/installed",
    operation_id = "mark_sale_installed",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id")),
    request_body = Installed,
    responses(
        (status = 204, description = "Marked installed"),
        (status = 400, description = "Already installed, no slabs, not all slabs cut, or a bad date", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop", body = ErrorBody),
        (status = 404, description = "No such sale in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn mark_installed(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<Installed>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    let installed: Option<Option<DateTime<Utc>>> =
        sqlx::query_scalar("SELECT installed_date FROM sales WHERE id = ? AND company_id = ?")
            .bind(id)
            .bind(user.company_id)
            .fetch_optional(pool)
            .await?;
    let installed = installed.ok_or(ApiError::NotFound)?;
    if installed.is_some() {
        return Err(ApiError::BadRequest(
            "Transaction is already marked as installed".into(),
        ));
    }
    let (slabs, uncut): (i64, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*), CAST(SUM(cut_date IS NULL) AS SIGNED) FROM slab_inventory WHERE sale_id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    if slabs == 0 {
        return Err(ApiError::BadRequest(
            "No slabs found for this transaction".into(),
        ));
    }
    if uncut.unwrap_or(0) > 0 {
        return Err(ApiError::BadRequest(
            "Cannot mark as installed - not all slabs are cut".into(),
        ));
    }
    let at = installed_at(input.installed_date.as_deref())?;
    if input.paid {
        let paid = sqlx::query("UPDATE sales SET installed_date = ?, paid_date = ? WHERE id = ?")
            .bind(at)
            .bind(at)
            .bind(id)
            .execute(pool)
            .await;
        match paid {
            Ok(_) => return Ok(StatusCode::NO_CONTENT),
            // Databases without the legacy `sales.paid_date` keep the date only.
            Err(sqlx::Error::Database(err)) if err.message().contains("Unknown column") => {}
            Err(err) => return Err(err.into()),
        }
    }
    sqlx::query("UPDATE sales SET installed_date = ? WHERE id = ?")
        .bind(at)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
