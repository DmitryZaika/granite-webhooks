//! A customer's projects: the customer, its parent and the parent's other
//! projects (customers linked with `parent_id`), each with its sales.
//!
//! Replaces `customerProjectsTab.server.ts`.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, FromRow, MySqlPool};
use std::collections::HashMap;
use utoipa::ToSchema;

use crate::serde_helpers::js_date;

/// The projects of a customer's family.
#[derive(Debug, Serialize, ToSchema)]
pub struct CustomerProjects {
    /// The parent of the family (the customer itself when it has none).
    pub parent_id: i32,
    /// The parent and its projects, by name.
    pub customers: Vec<ProjectCustomer>,
}

/// One customer of the family.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct ProjectCustomer {
    /// Customer id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Primary email address.
    pub email: Option<String>,
    /// Every address, primary first.
    #[sqlx(skip)]
    pub emails: Vec<String>,
    /// Phone.
    pub phone: Option<String>,
    /// Address.
    pub address: Option<String>,
    /// Sales rep's name.
    pub sales_rep_name: Option<String>,
    /// Stage of the newest live deal.
    pub deal_stage: Option<String>,
    /// Lead source.
    pub source: Option<String>,
    /// City.
    pub city: Option<String>,
    /// State.
    pub state: Option<String>,
    /// ZIP code.
    pub postal_code: Option<String>,
    /// Company name.
    pub company_name: Option<String>,
    /// Notes.
    pub notes: Option<String>,
    /// Sales to the customer.
    #[sqlx(skip)]
    pub sales: Vec<ProjectSale>,
}

/// A sale to a customer of the family.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct ProjectSale {
    /// Sale id.
    pub id: i32,
    /// Customer.
    pub customer_id: i32,
    /// Price, a decimal string.
    pub price: Option<String>,
    /// Address of the job.
    pub project_address: Option<String>,
    /// When it was sold.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub sale_date: Option<DateTime<Utc>>,
}

/// Body of `PUT /v1/customers/{id}/parent`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ParentInput {
    /// The customer this one becomes a project of.
    pub parent_id: i32,
}

async fn in_company(
    pool: &MySqlPool,
    company_id: i32,
    id: i32,
) -> Result<Option<Option<i32>>, sqlx::Error> {
    sqlx::query_scalar("SELECT parent_id FROM customers WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(company_id)
        .fetch_optional(pool)
        .await
}

/// List a customer's projects
///
/// The live customers of the family the customer belongs to (its parent and
/// the parent's projects, or the customer and its own projects), by name,
/// with their addresses, newest deal stage and sales.
#[utoipa::path(
    get,
    path = "/v1/customers/{id}/projects",
    operation_id = "list_customer_projects",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id")),
    responses(
        (status = 200, description = "The family", body = CustomerProjects),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn projects(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<CustomerProjects>> {
    let pool = &state.pool;
    let parent = in_company(pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let parent_id = parent.filter(|parent| *parent > 0).unwrap_or(id);
    let mut customers: Vec<ProjectCustomer> = sqlx::query_as(
        "SELECT c.id, c.name, pe.email, c.phone, c.address, u.name AS sales_rep_name,
                (SELECT dl.name FROM deals d JOIN deals_list dl ON d.list_id = dl.id
                 WHERE d.customer_id = c.id AND d.deleted_at IS NULL
                 ORDER BY d.id DESC LIMIT 1) AS deal_stage,
                c.source, c.city, c.state, c.postal_code, c.company_name, c.notes
         FROM customers c
         LEFT JOIN customers_emails pe ON pe.id = c.email_id AND pe.customer_id = c.id
         LEFT JOIN users u ON u.id = c.sales_rep
         WHERE (c.parent_id = ? OR c.id = ?) AND c.company_id = ? AND c.deleted_at IS NULL
         ORDER BY c.name, c.id",
    )
    .bind(parent_id)
    .bind(parent_id)
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    if customers.is_empty() {
        return Ok(Json(CustomerProjects {
            parent_id,
            customers,
        }));
    }
    let marks = vec!["?"; customers.len()].join(",");
    let mut emails_query = sqlx::query_as::<_, (i32, String)>(AssertSqlSafe(format!(
        "SELECT ce.customer_id, ce.email FROM customers_emails ce
         JOIN customers c ON c.id = ce.customer_id
         WHERE ce.customer_id IN ({marks})
         ORDER BY ce.customer_id, ce.id = c.email_id DESC, ce.created_at, ce.id"
    )));
    let mut sales_query = sqlx::query_as::<_, ProjectSale>(AssertSqlSafe(format!(
        "SELECT id, customer_id, CAST(price AS CHAR) AS price, project_address, sale_date
         FROM sales WHERE customer_id IN ({marks}) ORDER BY sale_date, id"
    )));
    for customer in &customers {
        emails_query = emails_query.bind(customer.id);
        sales_query = sales_query.bind(customer.id);
    }
    let mut emails: HashMap<i32, Vec<String>> = HashMap::new();
    for (customer_id, email) in emails_query.fetch_all(pool).await? {
        emails.entry(customer_id).or_default().push(email);
    }
    let mut sales: HashMap<i32, Vec<ProjectSale>> = HashMap::new();
    for sale in sales_query.fetch_all(pool).await? {
        sales.entry(sale.customer_id).or_default().push(sale);
    }
    for customer in &mut customers {
        customer.emails = emails
            .remove(&customer.id)
            .unwrap_or_else(|| customer.email.clone().into_iter().collect());
        customer.sales = sales.remove(&customer.id).unwrap_or_default();
    }
    Ok(Json(CustomerProjects {
        parent_id,
        customers,
    }))
}

/// Link a customer under a parent
///
/// Makes the customer a project of `parent_id` (both of the company).
#[utoipa::path(
    put,
    path = "/v1/customers/{id}/parent",
    operation_id = "set_customer_parent",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id")),
    request_body = ParentInput,
    responses(
        (status = 204, description = "Linked"),
        (status = 400, description = "A customer cannot be its own parent", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Customer or parent not in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_parent(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<ParentInput>,
) -> ApiResult<StatusCode> {
    if body.parent_id == id {
        return Err(ApiError::BadRequest(
            "A customer cannot be its own parent".into(),
        ));
    }
    let pool = &state.pool;
    in_company(pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    super::service::refuse_import_placeholder(pool, id).await?;
    in_company(pool, user.company_id, body.parent_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    sqlx::query("UPDATE customers SET parent_id = ? WHERE id = ? AND company_id = ?")
        .bind(body.parent_id)
        .bind(id)
        .bind(user.company_id)
        .execute(pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A sale to the customer, as the customer view lists it.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct CustomerSale {
    /// Sale id.
    pub id: i32,
    /// When it was sold.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub sale_date: Option<DateTime<Utc>>,
    /// Price, a decimal string.
    pub price: Option<String>,
    /// Seller's name.
    pub seller_name: Option<String>,
}

/// List a customer's sales
///
/// The company's sales to the customer, newest first, with the seller.
#[utoipa::path(
    get,
    path = "/v1/customers/{id}/sales",
    operation_id = "list_customer_sales",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id")),
    responses(
        (status = 200, description = "Sales", body = Vec<CustomerSale>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn sales(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<CustomerSale>>> {
    in_company(&state.pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(
        sqlx::query_as(
            "SELECT s.id, s.sale_date, CAST(s.price AS CHAR) AS price, u.name AS seller_name
             FROM sales s JOIN users u ON u.id = s.seller_id
             WHERE s.customer_id = ? AND s.company_id = ?
             ORDER BY s.sale_date DESC, s.id DESC",
        )
        .bind(id)
        .bind(user.company_id)
        .fetch_all(&state.pool)
        .await?,
    ))
}
