use super::queries::{
    create_type, delete_type, find_type, find_type_by_name, list_colors, list_stone_types,
    list_suppliers, rename_type, stones_using_type,
};
use super::schemas::{Color, StoneType, StoneTypeInUse, StoneTypeInput, Supplier};
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// List the company's suppliers
///
/// Returns every supplier of the caller's company, by name. Stones, sinks and
/// faucets refer to them by `supplier_id`.
#[utoipa::path(
    get,
    path = "/v1/suppliers",
    operation_id = "list_suppliers",
    tag = "catalog",
    responses(
        (status = 200, description = "Suppliers by name", body = Vec<Supplier>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn suppliers(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<Supplier>>> {
    Ok(Json(list_suppliers(&state.pool, user.company_id).await?))
}

/// List the stone colors
///
/// Returns every color a stone can be tagged with (the same list for all
/// companies), by name. Stones carry them as `color_ids`.
#[utoipa::path(
    get,
    path = "/v1/colors",
    operation_id = "list_colors",
    tag = "catalog",
    responses(
        (status = 200, description = "Colors by name", body = Vec<Color>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn colors(
    State(state): State<AppState>,
    EmployeeUser(_user): EmployeeUser,
) -> ApiResult<Json<Vec<Color>>> {
    Ok(Json(list_colors(&state.pool).await?))
}

/// List the company's stone types
///
/// Returns the materials stones of the company can have (`granite`,
/// `quartz`...), by name. A stone's `type` must be one of them.
#[utoipa::path(
    get,
    path = "/v1/stone-types",
    operation_id = "list_stone_types",
    tag = "catalog",
    responses(
        (status = 200, description = "Stone types by name", body = Vec<StoneType>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn stone_types(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<StoneType>>> {
    Ok(Json(list_stone_types(&state.pool, user.company_id).await?))
}

fn valid_name(input: &StoneTypeInput) -> Result<String, ApiError> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 255 {
        return Err(ApiError::BadRequest(
            "Enter a stone type name between 1 and 255 characters.".into(),
        ));
    }
    Ok(name.to_string())
}

const DUPLICATE: &str = "This stone type already exists.";

/// Add a stone type
///
/// Creates a type for the caller's company. Names are unique per company,
/// ignoring case.
#[utoipa::path(
    post,
    path = "/v1/stone-types",
    operation_id = "create_stone_type",
    tag = "catalog",
    request_body = StoneTypeInput,
    responses(
        (status = 201, description = "Type created", body = StoneType),
        (status = 400, description = "Empty or longer than 255 characters", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 409, description = "A type with this name exists", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_stone_type(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<StoneTypeInput>,
) -> ApiResult<(StatusCode, Json<StoneType>)> {
    let name = valid_name(&input)?;
    if find_type_by_name(&state.pool, user.company_id, &name, None)
        .await?
        .is_some()
    {
        return Err(ApiError::Conflict(DUPLICATE.into()));
    }
    let id = create_type(&state.pool, user.company_id, &name).await?;
    Ok((StatusCode::CREATED, Json(StoneType { id, name })))
}

/// Rename a stone type
///
/// Renames the type and sets the new name on every stone of the company that
/// had the old one.
#[utoipa::path(
    patch,
    path = "/v1/stone-types/{id}",
    operation_id = "rename_stone_type",
    tag = "catalog",
    params(("id" = i64, Path, description = "Stone type id")),
    request_body = StoneTypeInput,
    responses(
        (status = 200, description = "Type renamed", body = StoneType),
        (status = 400, description = "Empty or longer than 255 characters", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such type in the caller's company", body = ErrorBody),
        (status = 409, description = "Another type has this name", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn rename_stone_type(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<StoneTypeInput>,
) -> ApiResult<Json<StoneType>> {
    let name = valid_name(&input)?;
    let existing = find_type(&state.pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if find_type_by_name(&state.pool, user.company_id, &name, Some(id))
        .await?
        .is_some()
    {
        return Err(ApiError::Conflict(DUPLICATE.into()));
    }
    rename_type(&state.pool, user.company_id, &existing, &name).await?;
    Ok(Json(StoneType { id, name }))
}

/// Delete a stone type
///
/// Deletes a type no live stone uses. When stones still use it, answers 409
/// with those stones; delete or retype them first.
#[utoipa::path(
    delete,
    path = "/v1/stone-types/{id}",
    operation_id = "delete_stone_type",
    tag = "catalog",
    params(("id" = i64, Path, description = "Stone type id")),
    responses(
        (status = 204, description = "Type deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such type in the caller's company", body = ErrorBody),
        (status = 409, description = "Live stones use the type", body = StoneTypeInUse),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_stone_type(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    let existing = find_type(&state.pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let stones = stones_using_type(&state.pool, user.company_id, &existing.name).await?;
    if !stones.is_empty() {
        let body = StoneTypeInUse {
            error: "This type is used by undeleted stones and cannot be deleted.".into(),
            stones,
        };
        return Ok((StatusCode::CONFLICT, Json(body)).into_response());
    }
    delete_type(&state.pool, user.company_id, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
