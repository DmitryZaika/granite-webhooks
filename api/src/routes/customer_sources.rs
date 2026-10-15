//! Where customers come from: the company's source list.
//!
//! - `GET    /v1/customer-sources`      → built-in ones first, then by label
//! - `POST   /v1/customer-sources`      → add one (admins)
//! - `PUT    /v1/customer-sources/{id}` → rename (admins)
//! - `DELETE /v1/customer-sources/{id}` → hide a custom one (admins)
//!
//! Replaces the Remix `api.customer-sources` route.

use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::customers::service::BUILTIN_SOURCES;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(rename, delete))
}

/// A customer source.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct CustomerSource {
    /// The source.
    pub id: i64,
    /// Stored on customers (`customers.source`).
    pub source_key: String,
    /// Shown in the dropdown.
    pub label: String,
    /// Built in: can be renamed but not deleted.
    pub is_system: bool,
}

/// Body of `POST /v1/customer-sources` and `PUT /v1/customer-sources/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SourceName {
    /// 1 to 255 characters.
    pub name: String,
}

/// `POST /v1/customer-sources`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedSource {
    /// The new source.
    pub id: u64,
}

fn checked(name: &str) -> ApiResult<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 255 {
        return Err(ApiError::BadRequest(
            "Enter a source name between 1 and 255 characters.".into(),
        ));
    }
    Ok(name)
}

fn taken() -> ApiError {
    ApiError::Conflict("This source already exists.".into())
}

/// Another live source of the company has this label or key.
async fn label_taken(
    pool: &MySqlPool,
    company_id: i32,
    name: &str,
    except: i32,
) -> Result<bool, sqlx::Error> {
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM customer_sources
          WHERE company_id = ? AND deleted_at IS NULL AND id <> ?
            AND (LOWER(label) = LOWER(?) OR LOWER(source_key) = LOWER(?))
          LIMIT 1",
    )
    .bind(company_id)
    .bind(except)
    .bind(name)
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// List customer sources
///
/// The company's live sources, built-in ones first, then by label.
#[utoipa::path(
    get,
    path = "/v1/customer-sources",
    operation_id = "list_customer_sources",
    tag = "customers",
    responses(
        (status = 200, description = "Sources", body = [CustomerSource]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<CustomerSource>>> {
    let sources = sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, source_key, label, is_system <> 0 AS is_system FROM customer_sources
          WHERE company_id = ? AND deleted_at IS NULL
          ORDER BY is_system DESC, label ASC",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(sources))
}

/// Add a customer source
///
/// Its key and label are the name.
#[utoipa::path(
    post,
    path = "/v1/customer-sources",
    operation_id = "create_customer_source",
    tag = "customers",
    request_body = SourceName,
    responses(
        (status = 201, description = "Added", body = CreatedSource),
        (status = 400, description = "Empty or too long", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 409, description = "A source with that name exists", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<SourceName>,
) -> ApiResult<(StatusCode, Json<CreatedSource>)> {
    let name = checked(&input.name)?;
    if BUILTIN_SOURCES.contains(&name.to_lowercase().as_str())
        || label_taken(&state.pool, user.company_id, name, 0).await?
    {
        return Err(taken());
    }
    let id = sqlx::query(
        "INSERT INTO customer_sources (company_id, source_key, label, is_system) VALUES (?, ?, ?, 0)",
    )
    .bind(user.company_id)
    .bind(name)
    .bind(name)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((StatusCode::CREATED, Json(CreatedSource { id })))
}

/// The company's live source: its key and whether it is built in.
async fn find(pool: &MySqlPool, company_id: i32, id: i32) -> ApiResult<(String, bool)> {
    let found: Option<(String, bool)> = sqlx::query_as(
        "SELECT source_key, is_system <> 0 FROM customer_sources
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    found.ok_or(ApiError::NotFound)
}

/// Rename a customer source
///
/// A built-in source only changes its label; a custom one also changes its
/// key, and its customers move to the new name.
#[utoipa::path(
    put,
    path = "/v1/customer-sources/{id}",
    operation_id = "rename_customer_source",
    tag = "customers",
    params(("id" = i32, Path, description = "Source id")),
    request_body = SourceName,
    responses(
        (status = 204, description = "Renamed"),
        (status = 400, description = "Empty or too long", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such source in the caller's company", body = ErrorBody),
        (status = 409, description = "A source with that name exists", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn rename(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SourceName>,
) -> ApiResult<StatusCode> {
    let name = checked(&input.name)?;
    let (key, is_system) = find(&state.pool, user.company_id, id).await?;
    if label_taken(&state.pool, user.company_id, name, id).await? {
        return Err(taken());
    }
    let mut tx = state.pool.begin().await?;
    if is_system {
        sqlx::query("UPDATE customer_sources SET label = ? WHERE id = ?")
            .bind(name)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query("UPDATE customer_sources SET label = ?, source_key = ? WHERE id = ?")
            .bind(name)
            .bind(name)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE customers SET source = ?
              WHERE company_id = ? AND source = ? AND deleted_at IS NULL",
        )
        .bind(name)
        .bind(user.company_id)
        .bind(&key)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a customer source
///
/// Hides a custom source from the dropdown; its customers keep it.
#[utoipa::path(
    delete,
    path = "/v1/customer-sources/{id}",
    operation_id = "delete_customer_source",
    tag = "customers",
    params(("id" = i32, Path, description = "Source id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "A built-in source", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such source in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let (_, is_system) = find(&state.pool, user.company_id, id).await?;
    if is_system {
        return Err(ApiError::BadRequest(
            "Built-in sources can't be deleted.".into(),
        ));
    }
    sqlx::query("UPDATE customer_sources SET deleted_at = NOW() WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
