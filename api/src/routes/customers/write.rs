//! Creating and changing customers.
//!
//! Replaces `api.customers.create`, `api.customers.$customerId` (action),
//! `api.customers.set-sales-rep` and `api.customers.set-temperature`.

use super::service::{
    SALES_REP_POSITION, audit_name, ensure_open_deal, record_lead_manager_assignment,
    record_reassignment, refuse_import_placeholder, remind_new_rep, source_allowed, sync_emails,
    user_name,
};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

/// How a customer heard of the company.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ReferralSource {
    Google,
    Facebook,
    Referral,
    Flyer,
    DriveThru,
    Instagram,
    Other,
}

impl ReferralSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Facebook => "facebook",
            Self::Referral => "referral",
            Self::Flyer => "flyer",
            Self::DriveThru => "drive-thru",
            Self::Instagram => "instagram",
            Self::Other => "other",
        }
    }
}

/// Body of `POST /v1/customers` and `PUT /v1/customers/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CustomerInput {
    /// Contact name (required).
    pub name: String,
    /// Main phone.
    pub phone: Option<String>,
    /// Second phone.
    pub phone_2: Option<String>,
    /// Every email address, primary first (replaces the saved ones).
    #[serde(default)]
    pub emails: Vec<String>,
    /// Address.
    pub address: Option<String>,
    /// The customer's message.
    pub your_message: Option<String>,
    /// Company of a business customer.
    pub company_name: Option<String>,
    /// How they heard of the company (create only).
    pub referral_source: Option<ReferralSource>,
    /// Lead source: built in (`check-in`, `leads`, `call-in`, `other`) or one
    /// of the company's sources (1–255 characters).
    pub source: String,
    /// Assigned sales rep (user id), or null.
    pub sales_rep: Option<i32>,
}

/// Result of `POST /v1/customers`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedCustomer {
    /// New customer id.
    pub id: i32,
    /// The open deal of an assigned customer.
    pub deal_id: Option<i64>,
}

/// Body of `PUT /v1/customers/{id}/sales-rep`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SalesRepInput {
    /// The new sales rep (user id), or null to unassign.
    pub sales_rep: Option<i32>,
}

/// Lead temperature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Temperature {
    Hot,
    Medium,
    Cold,
}

impl Temperature {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Hot => "hot",
            Self::Medium => "medium",
            Self::Cold => "cold",
        }
    }
}

/// Query of `DELETE /v1/customers/{id}`.
#[derive(Debug, Default, Deserialize, crate::openapi::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeleteQuery {
    /// Also delete the customer's live deals (required when it has any).
    #[serde(default)]
    pub cascade_deals: bool,
}

/// Body of `PUT /v1/customers/{id}/temperature`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TemperatureInput {
    /// `hot`, `medium`, `cold`, or null to clear.
    pub temperature: Option<Temperature>,
}

fn blank_to_none(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn check(input: &CustomerInput) -> ApiResult<()> {
    if input.name.trim().is_empty() {
        return Err(ApiError::BadRequest("Name is required".into()));
    }
    let source = input.source.trim();
    if source.is_empty() || source.chars().count() > 255 {
        return Err(ApiError::BadRequest("Source is required".into()));
    }
    // Same rule as the form: an address is either empty or at least 3 characters.
    let address = blank_to_none(input.address.as_deref());
    if address.is_some_and(|address| address.chars().count() < 3) {
        return Err(ApiError::BadRequest(
            "Billing address must be at least 3 characters".into(),
        ));
    }
    Ok(())
}

/// A live user of the company may be a customer's sales rep.
async fn check_rep(state: &AppState, company_id: i32, rep: Option<i32>) -> ApiResult<()> {
    let Some(rep) = rep else {
        return Ok(());
    };
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
    )
    .bind(rep)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    found
        .map(|_| ())
        .ok_or_else(|| ApiError::BadRequest("Unknown sales rep".into()))
}

/// Create a customer
///
/// Saves a customer with its addresses (an address of another customer is
/// refused). Without a sales rep, a sales rep creating a customer (other
/// than a walk-in) becomes its rep. An assigned customer gets an open deal
/// and an audit line; the telephony address books are updated.
#[crate::openapi::path(
    post,
    path = "/v1/customers",
    operation_id = "create_customer",
    tag = "customers",
    request_body = CustomerInput,
    responses(
        (status = 201, description = "Created", body = CreatedCustomer),
        (status = 400, description = "Missing name, unknown source or sales rep", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 409, description = "An address belongs to another customer", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<CustomerInput>,
) -> ApiResult<(StatusCode, Json<CreatedCustomer>)> {
    check(&input)?;
    let pool = &state.pool;
    if !source_allowed(pool, user.company_id, &input.source, None).await? {
        return Err(ApiError::BadRequest("Select a valid source.".into()));
    }
    check_rep(&state, user.company_id, input.sales_rep).await?;
    let mut sales_rep = input.sales_rep;
    if sales_rep.is_none() && input.source.trim() != "check-in" {
        let is_rep: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM users_positions WHERE user_id = ? AND position_id = ? LIMIT 1",
        )
        .bind(user.id)
        .bind(SALES_REP_POSITION)
        .fetch_optional(pool)
        .await?;
        sales_rep = is_rep.map(|_| user.id);
    }
    let created_by = audit_name(user.name.as_deref());
    let mut tx = pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO customers (name, phone, phone_2, address, your_message, referral_source, source,
                                company_id, company_name, sales_rep, assigned_date, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, IF(? IS NULL, NULL, NOW()), ?)",
    )
    .bind(input.name.trim())
    .bind(blank_to_none(input.phone.as_deref()))
    .bind(blank_to_none(input.phone_2.as_deref()))
    .bind(blank_to_none(input.address.as_deref()))
    .bind(blank_to_none(input.your_message.as_deref()))
    .bind(input.referral_source.map(ReferralSource::as_str))
    .bind(input.source.trim())
    .bind(user.company_id)
    .bind(blank_to_none(input.company_name.as_deref()))
    .bind(sales_rep)
    .bind(sales_rep)
    .bind(&created_by)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let id = i32::try_from(id).unwrap_or(i32::MAX);
    sync_emails(&mut tx, id, &input.emails, user.id).await?;
    tx.commit().await?;

    if sales_rep.is_some() && created_by.is_some() {
        let to = user_name(pool, sales_rep).await?;
        record_reassignment(pool, id, created_by.as_deref(), to.as_deref(), None).await?;
    }
    let deal_id = match sales_rep {
        Some(rep) => Some(ensure_open_deal(pool, id, user.company_id, rep).await?),
        None => None,
    };
    state.webhooks.sync_contacts(user.company_id, id).await;
    Ok((StatusCode::CREATED, Json(CreatedCustomer { id, deal_id })))
}

/// Change a customer
///
/// Saves the contact details, source and addresses (exactly `emails`, the
/// first one primary). A new sales rep is recorded in the audit trail (with
/// the sales manager's original assignment of a lead first), stamps the
/// first assignment date, and gets the open deal.
#[crate::openapi::path(
    put,
    path = "/v1/customers/{id}",
    operation_id = "update_customer",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id")),
    request_body = CustomerInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Missing name, unknown source or sales rep", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live customer in the company", body = ErrorBody),
        (status = 409, description = "An address belongs to another customer", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<CustomerInput>,
) -> ApiResult<StatusCode> {
    check(&input)?;
    let pool = &state.pool;
    let previous: Option<(Option<i32>, Option<String>)> = sqlx::query_as(
        "SELECT sales_rep, source FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    let (previous_rep, current_source) = previous.ok_or(ApiError::NotFound)?;
    refuse_import_placeholder(pool, id).await?;
    if !source_allowed(
        pool,
        user.company_id,
        &input.source,
        current_source.as_deref(),
    )
    .await?
    {
        return Err(ApiError::BadRequest("Select a valid source.".into()));
    }
    check_rep(&state, user.company_id, input.sales_rep).await?;
    let rep_changed = previous_rep != input.sales_rep;
    if rep_changed {
        record_lead_manager_assignment(pool, id, previous_rep).await?;
    }
    let stamp = if rep_changed && input.sales_rep.is_some() {
        ", assigned_date = COALESCE(assigned_date, NOW())"
    } else {
        ""
    };
    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE customers SET name = ?, phone = ?, phone_2 = ?, address = ?, your_message = ?,
                source = ?, company_name = ?, sales_rep = ?{stamp}
         WHERE id = ? AND company_id = ? AND deleted_at IS NULL"
    )))
    .bind(input.name.trim())
    .bind(blank_to_none(input.phone.as_deref()))
    .bind(blank_to_none(input.phone_2.as_deref()))
    .bind(blank_to_none(input.address.as_deref()))
    .bind(blank_to_none(input.your_message.as_deref()))
    .bind(input.source.trim())
    .bind(blank_to_none(input.company_name.as_deref()))
    .bind(input.sales_rep)
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    // The customer's quotes take the phone and address only where theirs is empty.
    sqlx::query(
        "UPDATE quotes
         SET phone = IF(phone IS NULL OR TRIM(phone) = '', COALESCE(?, phone), phone),
             address = IF(address IS NULL OR TRIM(address) = '', COALESCE(?, address), address)
         WHERE customer_id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(blank_to_none(input.phone.as_deref()))
    .bind(blank_to_none(input.address.as_deref()))
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    sync_emails(&mut tx, id, &input.emails, user.id).await?;
    tx.commit().await?;

    if rep_changed {
        let to = user_name(pool, input.sales_rep).await?;
        let by = audit_name(user.name.as_deref());
        record_reassignment(pool, id, by.as_deref(), to.as_deref(), None).await?;
        if let Some(rep) = input.sales_rep {
            ensure_open_deal(pool, id, user.company_id, rep).await?;
        }
    }
    state.webhooks.sync_contacts(user.company_id, id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Assign a customer's sales rep
///
/// Records the change in the audit trail, stamps the first assignment
/// date, gives the rep the open deal and two reminders (text within a
/// day, check within three).
#[crate::openapi::path(
    put,
    path = "/v1/customers/{id}/sales-rep",
    operation_id = "set_customer_sales_rep",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id")),
    request_body = SalesRepInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Unknown sales rep", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live customer in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_sales_rep(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<SalesRepInput>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    let previous: Option<Option<i32>> = sqlx::query_scalar(
        "SELECT sales_rep FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    let previous = previous.ok_or(ApiError::NotFound)?;
    refuse_import_placeholder(pool, id).await?;
    check_rep(&state, user.company_id, body.sales_rep).await?;
    if previous != body.sales_rep {
        record_lead_manager_assignment(pool, id, previous).await?;
        let stamp = if body.sales_rep.is_some() {
            ", assigned_date = COALESCE(assigned_date, NOW())"
        } else {
            ""
        };
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE customers SET sales_rep = ?{stamp} WHERE id = ? AND deleted_at IS NULL"
        )))
        .bind(body.sales_rep)
        .bind(id)
        .execute(pool)
        .await?;
        let to = user_name(pool, body.sales_rep).await?;
        let by = audit_name(user.name.as_deref());
        record_reassignment(pool, id, by.as_deref(), to.as_deref(), None).await?;
    }
    if let Some(rep) = body.sales_rep {
        ensure_open_deal(pool, id, user.company_id, rep).await?;
        remind_new_rep(pool, rep, id).await?;
        state.webhooks.sync_contacts(user.company_id, id).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Set a lead's temperature
///
/// `hot`, `medium` or `cold`, or null to clear it.
#[crate::openapi::path(
    put,
    path = "/v1/customers/{id}/temperature",
    operation_id = "set_customer_temperature",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id")),
    request_body = TemperatureInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Unknown temperature", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live customer in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_temperature(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<TemperatureInput>,
) -> ApiResult<StatusCode> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    found.ok_or(ApiError::NotFound)?;
    refuse_import_placeholder(&state.pool, id).await?;
    sqlx::query("UPDATE customers SET customer_temperature = ? WHERE id = ? AND company_id = ?")
        .bind(body.temperature.map(Temperature::as_str))
        .bind(id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `positions.id` of a sales manager (may delete customers).
const SALES_MANAGER_POSITION: i32 = 2;

/// Admins and the company's sales managers manage customers' fate.
async fn require_manager(
    pool: &sqlx::MySqlPool,
    user: &crate::auth::session::SessionUser,
) -> ApiResult<()> {
    if user.is_admin || user.is_superuser {
        return Ok(());
    }
    let manager: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM users_positions WHERE user_id = ? AND company_id = ? AND position_id = ? LIMIT 1",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(SALES_MANAGER_POSITION)
    .fetch_optional(pool)
    .await?;
    manager.map(|_| ()).ok_or(ApiError::Forbidden)
}

/// Body of `PUT /v1/customers/{id}/invalid`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InvalidInput {
    /// Why the lead is invalid, e.g. `Out of area` (1–255 characters).
    pub reason: String,
}

/// Mark a lead invalid
///
/// Records why the lead is invalid, unassigns it and hides its live deals
/// (each deletion audited). Admins and sales managers only.
#[crate::openapi::path(
    put,
    path = "/v1/customers/{id}/invalid",
    operation_id = "mark_customer_invalid",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id")),
    request_body = InvalidInput,
    responses(
        (status = 204, description = "Marked"),
        (status = 400, description = "Missing reason", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is neither an admin nor a sales manager", body = ErrorBody),
        (status = 404, description = "No such live customer in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn mark_invalid(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<InvalidInput>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    require_manager(pool, &user).await?;
    let reason = body.reason.trim();
    if reason.is_empty() || reason.chars().count() > 255 {
        return Err(ApiError::BadRequest("Select a reason".into()));
    }
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    found.ok_or(ApiError::NotFound)?;
    refuse_import_placeholder(pool, id).await?;
    let deals: Vec<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM deals WHERE customer_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let by = audit_name(user.name.as_deref());
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE customers SET invalid_lead = ?, sales_rep = NULL WHERE id = ? AND company_id = ?",
    )
    .bind(reason)
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE deals SET deleted_at = NOW() WHERE customer_id = ? AND deleted_at IS NULL")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if let Some(by) = &by {
        for deal_id in deals {
            sqlx::query(
                "INSERT INTO customers_history (customer_id, reassigned_by, reassigned_to) VALUES (?, ?, ?)",
            )
            .bind(id)
            .bind(by)
            .bind(format!("deal_deleted:{deal_id}"))
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a customer
///
/// Hides the customer (soft delete) and removes it from the telephony
/// address books. A customer with live deals needs `cascade_deals=true`,
/// which hides the deals too. Admins and sales managers only.
#[crate::openapi::path(
    delete,
    path = "/v1/customers/{id}",
    operation_id = "delete_customer",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id"), DeleteQuery),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is neither an admin nor a sales manager", body = ErrorBody),
        (status = 404, description = "No such live customer in the company", body = ErrorBody),
        (status = 409, description = "The customer has live deals and `cascade_deals` is not set", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    Query(query): Query<DeleteQuery>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    require_manager(pool, &user).await?;
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    found.ok_or(ApiError::NotFound)?;
    refuse_import_placeholder(pool, id).await?;
    let deals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deals WHERE customer_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    if deals > 0 && !query.cascade_deals {
        return Err(ApiError::Conflict(
            "Confirm removing associated deals".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    if query.cascade_deals {
        sqlx::query(
            "UPDATE deals SET deleted_at = NOW() WHERE customer_id = ? AND deleted_at IS NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE customers SET deleted_at = NOW() WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(user.company_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    state.webhooks.remove_contacts(user.company_id, id).await;
    Ok(StatusCode::NO_CONTENT)
}
