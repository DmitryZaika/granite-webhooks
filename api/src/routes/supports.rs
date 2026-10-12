//! Supports: pictures of the countertop supports the company installs.
//!
//! - `GET    /v1/supports`      → the company's supports, by name
//! - `POST   /v1/supports`      → add one (picture uploaded to `supports/`)
//! - `PUT    /v1/supports/{id}` → rename it, or replace its picture
//! - `DELETE /v1/supports/{id}` → delete it
//!
//! Replaces the Remix `employee.supports` loader and the `admin.supports.*`
//! loaders and actions.

use super::attachable::LibraryFile;
use super::uploads::{self, schemas::UploadFolder};
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(update, delete))
}

/// Body of `POST /v1/supports`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewSupport {
    /// Name (1–100 characters).
    pub name: String,
    /// URL of the picture uploaded to the `supports` folder.
    pub url: String,
}

/// Body of `PUT /v1/supports/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SupportUpdate {
    /// New name (1–100 characters).
    pub name: String,
    /// A newly uploaded picture replacing the old one; leave out to keep it.
    #[serde(default)]
    pub url: Option<String>,
}

fn checked_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(ApiError::BadRequest(
            "Name is required (at most 100 characters)".into(),
        ));
    }
    Ok(name.to_string())
}

async fn find(state: &AppState, company_id: i32, id: i32) -> ApiResult<LibraryFile> {
    sqlx::query_as("SELECT id, name, url FROM supports WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(company_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(ApiError::NotFound)
}

/// List the supports
///
/// The company's supports with their pictures (thumbnails have `-icon`
/// before the extension), by name.
#[utoipa::path(
    get,
    path = "/v1/supports",
    operation_id = "list_supports",
    tag = "catalog",
    responses(
        (status = 200, description = "Supports by name", body = Vec<LibraryFile>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<LibraryFile>>> {
    Ok(Json(
        sqlx::query_as("SELECT id, name, url FROM supports WHERE company_id = ? ORDER BY name, id")
            .bind(user.company_id)
            .fetch_all(&state.pool)
            .await?,
    ))
}

/// Add a support
///
/// A picture uploaded to the `supports` folder (`POST /v1/uploads`) under
/// `name`; its thumbnail is made now.
#[utoipa::path(
    post,
    path = "/v1/supports",
    operation_id = "create_support",
    tag = "catalog",
    request_body = NewSupport,
    responses(
        (status = 201, description = "Added", body = LibraryFile),
        (status = 400, description = "No name, or not an uploaded picture", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<NewSupport>,
) -> ApiResult<(StatusCode, Json<LibraryFile>)> {
    let name = checked_name(&input.name)?;
    uploads::attach(&state.storage, &input.url, UploadFolder::Supports).await?;
    let id = sqlx::query("INSERT INTO supports (name, url, company_id) VALUES (?, ?, ?)")
        .bind(&name)
        .bind(&input.url)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?
        .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(LibraryFile {
            id: i32::try_from(id).unwrap_or(i32::MAX),
            name: Some(name),
            url: Some(input.url),
        }),
    ))
}

/// Edit a support
///
/// Renames it; with `url`, its picture is replaced by a new upload and the
/// old picture and thumbnail are deleted from storage.
#[utoipa::path(
    put,
    path = "/v1/supports/{id}",
    operation_id = "update_support",
    tag = "catalog",
    params(("id" = i32, Path, description = "Support id")),
    request_body = SupportUpdate,
    responses(
        (status = 200, description = "Saved", body = LibraryFile),
        (status = 400, description = "No name, or not an uploaded picture", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such support in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SupportUpdate>,
) -> ApiResult<Json<LibraryFile>> {
    let name = checked_name(&input.name)?;
    let old = find(&state, user.company_id, id).await?;
    let new_url = input.url.filter(|url| Some(url) != old.url.as_ref());
    if let Some(url) = &new_url {
        uploads::attach(&state.storage, url, UploadFolder::Supports).await?;
    }
    sqlx::query(
        "UPDATE supports SET name = ?, url = COALESCE(?, url) WHERE id = ? AND company_id = ?",
    )
    .bind(&name)
    .bind(&new_url)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    if let (Some(_), Some(old_url)) = (&new_url, &old.url) {
        uploads::remove(&state.storage, old_url).await;
    }
    Ok(Json(find(&state, user.company_id, id).await?))
}

/// Delete a support
///
/// Removes it from the company's supports.
#[utoipa::path(
    delete,
    path = "/v1/supports/{id}",
    operation_id = "delete_support",
    tag = "catalog",
    params(("id" = i32, Path, description = "Support id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such support in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let deleted = sqlx::query("DELETE FROM supports WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
