//! Where a company's leads come from (`marketing_refferal_sources`): the
//! choices of the add-lead form, managed by the marketing team.

use super::require_marketer;
use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// A referral source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct ReferralSource {
    /// Source id.
    pub id: i64,
    /// Name, e.g. `facebook` or `LSA`.
    pub name: String,
}

/// Body of `POST` and `PATCH` on referral sources.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReferralSourceInput {
    /// 1 to 255 characters; unique per company, ignoring case.
    pub name: String,
}

/// The live referral sources of the company, by name.
pub async fn company_sources(
    state: &AppState,
    company_id: i32,
) -> Result<Vec<ReferralSource>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, name FROM marketing_refferal_sources
          WHERE company_id = ? AND deleted_at IS NULL ORDER BY name ASC, id ASC",
    )
    .bind(company_id)
    .fetch_all(&state.pool)
    .await
}

/// A live source of the company with this name (case ignored), other than `except`.
pub async fn source_named(
    state: &AppState,
    company_id: i32,
    name: &str,
    except: Option<i64>,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM marketing_refferal_sources
          WHERE company_id = ? AND deleted_at IS NULL AND LOWER(name) = LOWER(?) AND id <> ?
          LIMIT 1",
    )
    .bind(company_id)
    .bind(name.trim())
    .bind(except.unwrap_or(-1))
    .fetch_optional(&state.pool)
    .await
}

fn valid_name(input: &ReferralSourceInput) -> Result<String, ApiError> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 255 {
        return Err(ApiError::BadRequest(
            "Enter a referral source name between 1 and 255 characters.".into(),
        ));
    }
    Ok(name.to_string())
}

const DUPLICATE: &str = "This referral source already exists.";

/// List referral sources
///
/// The company's referral sources, by name: the choices for a new lead.
#[crate::openapi::path(
    get,
    path = "/v1/marketing/{company_id}/referral-sources",
    operation_id = "list_marketing_referral_sources",
    tag = "marketing",
    params(("company_id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "Sources by name", body = Vec<ReferralSource>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not on the company's marketing team", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(company_id): Path<i32>,
) -> ApiResult<Json<Vec<ReferralSource>>> {
    require_marketer(&state, &user, company_id).await?;
    Ok(Json(company_sources(&state, company_id).await?))
}

/// Add a referral source
///
/// Adds a source the company's leads can come from.
#[crate::openapi::path(
    post,
    path = "/v1/marketing/{company_id}/referral-sources",
    operation_id = "create_marketing_referral_source",
    tag = "marketing",
    params(("company_id" = i32, Path, description = "Company id")),
    request_body = ReferralSourceInput,
    responses(
        (status = 201, description = "Added", body = ReferralSource),
        (status = 400, description = "Empty or longer than 255 characters", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not on the company's marketing team", body = ErrorBody),
        (status = 409, description = "A source with this name exists", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(company_id): Path<i32>,
    JsonBody(input): JsonBody<ReferralSourceInput>,
) -> ApiResult<(StatusCode, Json<ReferralSource>)> {
    require_marketer(&state, &user, company_id).await?;
    let name = valid_name(&input)?;
    if source_named(&state, company_id, &name, None)
        .await?
        .is_some()
    {
        return Err(ApiError::Conflict(DUPLICATE.into()));
    }
    let id = sqlx::query("INSERT INTO marketing_refferal_sources (company_id, name) VALUES (?, ?)")
        .bind(company_id)
        .bind(&name)
        .execute(&state.pool)
        .await?
        .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(ReferralSource {
            id: i64::try_from(id).unwrap_or(i64::MAX),
            name,
        }),
    ))
}

async fn existing(state: &AppState, company_id: i32, id: i64) -> Result<(), ApiError> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM marketing_refferal_sources WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// Rename a referral source
///
/// Renames a source; leads keep the name they were saved with.
#[crate::openapi::path(
    patch,
    path = "/v1/marketing/{company_id}/referral-sources/{id}",
    operation_id = "rename_marketing_referral_source",
    tag = "marketing",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("id" = i64, Path, description = "Source id"),
    ),
    request_body = ReferralSourceInput,
    responses(
        (status = 200, description = "Renamed", body = ReferralSource),
        (status = 400, description = "Empty or longer than 255 characters", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not on the company's marketing team", body = ErrorBody),
        (status = 404, description = "No such source", body = ErrorBody),
        (status = 409, description = "Another source has this name", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn rename(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((company_id, id)): Path<(i32, i64)>,
    JsonBody(input): JsonBody<ReferralSourceInput>,
) -> ApiResult<Json<ReferralSource>> {
    require_marketer(&state, &user, company_id).await?;
    let name = valid_name(&input)?;
    existing(&state, company_id, id).await?;
    if source_named(&state, company_id, &name, Some(id))
        .await?
        .is_some()
    {
        return Err(ApiError::Conflict(DUPLICATE.into()));
    }
    sqlx::query("UPDATE marketing_refferal_sources SET name = ? WHERE id = ? AND company_id = ?")
        .bind(&name)
        .bind(id)
        .bind(company_id)
        .execute(&state.pool)
        .await?;
    Ok(Json(ReferralSource { id, name }))
}

/// Delete a referral source
///
/// Hides a source from the add-lead form; leads keep it.
#[crate::openapi::path(
    delete,
    path = "/v1/marketing/{company_id}/referral-sources/{id}",
    operation_id = "delete_marketing_referral_source",
    tag = "marketing",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("id" = i64, Path, description = "Source id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not on the company's marketing team", body = ErrorBody),
        (status = 404, description = "No such source", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((company_id, id)): Path<(i32, i64)>,
) -> ApiResult<StatusCode> {
    require_marketer(&state, &user, company_id).await?;
    existing(&state, company_id, id).await?;
    sqlx::query(
        "UPDATE marketing_refferal_sources SET deleted_at = NOW() WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(company_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
