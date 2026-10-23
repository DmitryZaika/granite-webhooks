//! Sales not yet invoiced in QuickBooks (the admin Invoices page).
//!
//! - `GET /v1/invoice-queue`            → the company's sales without an invoice
//! - `GET /v1/invoice-queue/{sale_id}`  → one of them, with its customer's contact
//!
//! Replaces the Remix `admin.invoices` and `admin.invoices.$saleId` loaders.

use crate::auth::AdminUser;
use crate::entitlements::AddonKey;
use crate::error::{AddonRequiredBody, ApiError, ApiResult, ErrorBody};
use crate::serde_helpers::js_date;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(get))
}

/// A sale waiting for its invoice.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct QueuedSale {
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
    /// When it was cancelled.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub cancelled_date: Option<DateTime<Utc>>,
    /// When it was installed.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub installed_date: Option<DateTime<Utc>>,
}

/// A queued sale with its customer's contact.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct QueuedSaleDetail {
    #[serde(flatten)]
    #[sqlx(flatten)]
    pub sale: QueuedSale,
    /// The customer's primary email.
    pub email: Option<String>,
    /// The customer's phone.
    pub phone: Option<String>,
    /// The customer's address.
    pub address: Option<String>,
    /// The customer's QuickBooks id; null until linked.
    pub qbo_id: Option<i32>,
}

const COLUMNS: &str = "s.id, s.sale_date, c.name AS customer_name, u.name AS seller_name,
    s.cancelled_date, s.installed_date";

/// List sales to invoice
///
/// The company's sales with no QuickBooks invoice yet, newest first.
#[utoipa::path(
    get,
    path = "/v1/invoice-queue",
    operation_id = "list_invoice_queue",
    tag = "quickbooks",
    responses(
        (status = 200, description = "Sales without an invoice", body = Vec<QueuedSale>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the QuickBooks add-on (`addon`: `quickbooks`)", body = AddonRequiredBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<QueuedSale>>> {
    user.plan.require(AddonKey::Quickbooks)?;
    Ok(Json(
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM sales s
               JOIN customers c ON s.customer_id = c.id
               JOIN users u ON s.seller_id = u.id
              WHERE s.company_id = ? AND s.qbo_id IS NULL
              ORDER BY s.sale_date DESC, s.id DESC"
        )))
        .bind(user.company_id)
        .fetch_all(&state.pool)
        .await?,
    ))
}

/// Get a sale to invoice
///
/// One sale of the company with its customer's email, phone, address and
/// QuickBooks link, to review before invoicing.
#[utoipa::path(
    get,
    path = "/v1/invoice-queue/{sale_id}",
    operation_id = "get_invoice_queue_sale",
    tag = "quickbooks",
    params(("sale_id" = i32, Path, description = "Sale id")),
    responses(
        (status = 200, description = "The sale", body = QueuedSaleDetail),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the QuickBooks add-on (`addon`: `quickbooks`)", body = AddonRequiredBody),
        (status = 404, description = "No such sale in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(sale_id): Path<i32>,
) -> ApiResult<Json<QueuedSaleDetail>> {
    user.plan.require(AddonKey::Quickbooks)?;
    let row: Option<QueuedSaleDetail> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS}, primary_email.email AS email, c.phone, c.address, c.qbo_id
           FROM sales s
           JOIN customers c ON s.customer_id = c.id
           LEFT JOIN customers_emails primary_email
             ON primary_email.id = c.email_id AND primary_email.customer_id = c.id
           JOIN users u ON s.seller_id = u.id
          WHERE s.id = ? AND s.company_id = ?"
    )))
    .bind(sale_id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(Json(row.ok_or(ApiError::NotFound)?))
}
