use super::queries::{
    SINK_TYPES, STONE_TYPES, TypeTable, create_type, delete_type, find_type, find_type_by_name,
    items_using_type, list_colors, list_suppliers, list_types, rename_type,
};
use super::schemas::{
    Color, SinkTypeInUse, StoneType, StoneTypeInUse, StoneTypeInput, Supplier, SupplierEntry,
    SupplierFile,
};
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

/// The supplier directory
///
/// Every supplier of the caller's company with its contact details and files,
/// by id. The onboarding sample supplier is the page's to place.
#[utoipa::path(
    get,
    path = "/v1/suppliers/directory",
    operation_id = "list_supplier_directory",
    tag = "catalog",
    responses(
        (status = 200, description = "Suppliers with files", body = Vec<SupplierEntry>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn supplier_directory(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<SupplierEntry>>> {
    let mut suppliers: Vec<SupplierEntry> = sqlx::query_as(
        "SELECT id, supplier_name, manager, phone, email, notes, website
           FROM suppliers WHERE company_id = ? ORDER BY id",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    let files: Vec<SupplierFile> = sqlx::query_as(
        "SELECT CAST(f.id AS SIGNED) AS id, f.supplier_id, f.name, f.url
           FROM supplier_files f JOIN suppliers s ON s.id = f.supplier_id
          WHERE s.company_id = ? ORDER BY f.id",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    for file in files {
        if let Some(supplier) = suppliers.iter_mut().find(|s| s.id == file.supplier_id) {
            supplier.files.push(file);
        }
    }
    Ok(Json(suppliers))
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

fn valid_name(table: TypeTable, input: &StoneTypeInput) -> Result<String, ApiError> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 255 {
        return Err(ApiError::BadRequest(format!(
            "Enter a {} type name between 1 and 255 characters.",
            table.label
        )));
    }
    Ok(name.to_string())
}

fn duplicate(table: TypeTable) -> ApiError {
    ApiError::Conflict(format!("This {} type already exists.", table.label))
}

async fn create(
    state: &AppState,
    table: TypeTable,
    company_id: i32,
    input: &StoneTypeInput,
) -> ApiResult<(StatusCode, Json<StoneType>)> {
    let name = valid_name(table, input)?;
    if find_type_by_name(&state.pool, table, company_id, &name, None)
        .await?
        .is_some()
    {
        return Err(duplicate(table));
    }
    let id = create_type(&state.pool, table, company_id, &name).await?;
    Ok((StatusCode::CREATED, Json(StoneType { id, name })))
}

async fn rename(
    state: &AppState,
    table: TypeTable,
    company_id: i32,
    id: i64,
    input: &StoneTypeInput,
) -> ApiResult<Json<StoneType>> {
    let name = valid_name(table, input)?;
    let existing = find_type(&state.pool, table, company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if find_type_by_name(&state.pool, table, company_id, &name, Some(id))
        .await?
        .is_some()
    {
        return Err(duplicate(table));
    }
    rename_type(&state.pool, table, company_id, &existing, &name).await?;
    Ok(Json(StoneType { id, name }))
}

/// Deletes an unused type; `in_use` makes the 409 body from the items using it.
async fn delete<B: serde::Serialize>(
    state: &AppState,
    table: TypeTable,
    company_id: i32,
    id: i64,
    in_use: impl FnOnce(String, Vec<super::schemas::StoneUsingType>) -> B,
) -> ApiResult<Response> {
    let existing = find_type(&state.pool, table, company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let items = items_using_type(&state.pool, table, company_id, &existing.name).await?;
    if !items.is_empty() {
        let error = format!(
            "This type is used by undeleted {} and cannot be deleted.",
            table.plural
        );
        return Ok((StatusCode::CONFLICT, Json(in_use(error, items))).into_response());
    }
    delete_type(&state.pool, table, company_id, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
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
    Ok(Json(
        list_types(&state.pool, STONE_TYPES, user.company_id).await?,
    ))
}

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
    create(&state, STONE_TYPES, user.company_id, &input).await
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
    rename(&state, STONE_TYPES, user.company_id, id, &input).await
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
    delete(&state, STONE_TYPES, user.company_id, id, |error, stones| {
        StoneTypeInUse { error, stones }
    })
    .await
}

/// List the company's sink types
///
/// Returns the sink types of the company (`composite`, `farm house`...), by
/// name. A sink model's `type` must be one of them.
#[utoipa::path(
    get,
    path = "/v1/sink-types",
    operation_id = "list_sink_types",
    tag = "catalog",
    responses(
        (status = 200, description = "Sink types by name", body = Vec<StoneType>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn sink_types(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<StoneType>>> {
    Ok(Json(
        list_types(&state.pool, SINK_TYPES, user.company_id).await?,
    ))
}

/// Add a sink type
///
/// Creates a sink type for the caller's company. Names are unique per
/// company, ignoring case.
#[utoipa::path(
    post,
    path = "/v1/sink-types",
    operation_id = "create_sink_type",
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
pub async fn create_sink_type(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<StoneTypeInput>,
) -> ApiResult<(StatusCode, Json<StoneType>)> {
    create(&state, SINK_TYPES, user.company_id, &input).await
}

/// Rename a sink type
///
/// Renames the type and sets the new name on every sink model of the company
/// that had the old one.
#[utoipa::path(
    patch,
    path = "/v1/sink-types/{id}",
    operation_id = "rename_sink_type",
    tag = "catalog",
    params(("id" = i64, Path, description = "Sink type id")),
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
pub async fn rename_sink_type(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<StoneTypeInput>,
) -> ApiResult<Json<StoneType>> {
    rename(&state, SINK_TYPES, user.company_id, id, &input).await
}

/// Delete a sink type
///
/// Deletes a type no live sink model uses. When models still use it, answers
/// 409 with those models; delete or retype them first.
#[utoipa::path(
    delete,
    path = "/v1/sink-types/{id}",
    operation_id = "delete_sink_type",
    tag = "catalog",
    params(("id" = i64, Path, description = "Sink type id")),
    responses(
        (status = 204, description = "Type deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such type in the caller's company", body = ErrorBody),
        (status = 409, description = "Live sink models use the type", body = SinkTypeInUse),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_sink_type(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    delete(&state, SINK_TYPES, user.company_id, id, |error, sinks| {
        SinkTypeInUse { error, sinks }
    })
    .await
}
