use super::queries::list_sinks;
use super::schemas::{ListSinksQuery, Sink};
use crate::auth::{AdminUser, EmployeeUser, ShopOrEmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::routes::fixtures::{
    self, AddPhotos, CreatedModel, FixtureInput, FixtureModel, FixturePhoto,
};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

/// List the company's sink models with free units
///
/// Returns the non-deleted sink models of the caller's company, sorted by
/// name, each with the number of units free to sell (`available`). Models
/// not on display are included (`is_display`). Sold out models (no free unit
/// and not regular stock) are hidden unless `include_sold_out=true`. Filters
/// combine with AND; `type` takes the parameter repeated and matches any value.
#[utoipa::path(
    get,
    path = "/v1/sinks",
    operation_id = "list_sinks",
    tag = "sinks",
    params(ListSinksQuery),
    responses(
        (status = 200, description = "Sink models sorted by name", body = Vec<Sink>),
        (status = 400, description = "A filter has the wrong type, e.g. `supplier_id=abc`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ListSinksQuery>,
) -> ApiResult<Json<Vec<Sink>>> {
    Ok(Json(
        list_sinks(&state.pool, user.company_id, &query).await?,
    ))
}

// ---- Model detail and admin writes (shared code in `routes::fixtures`)

/// Get one sink model
///
/// Returns a live sink model of the caller's company with its unit counts
/// (`units` includes the `installed_units` on sales).
#[utoipa::path(
    get,
    path = "/v1/sinks/{id}",
    operation_id = "get_sink",
    tag = "sinks",
    params(("id" = i32, Path, description = "Sink model id")),
    responses(
        (status = 200, description = "The model", body = FixtureModel),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such model in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<FixtureModel>> {
    let mut conn = state.pool.acquire().await?;
    fixtures::find(&mut conn, fixtures::SINKS, user.company_id, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// Add a sink model
///
/// Creates a sink model of the caller's company with its image (an upload
/// to the `sinks` folder; the thumbnail is made here) and, with `units`,
/// that many units in stock.
#[utoipa::path(
    post,
    path = "/v1/sinks",
    operation_id = "create_sink",
    tag = "sinks",
    request_body = FixtureInput,
    responses(
        (status = 201, description = "Model created", body = CreatedModel),
        (status = 400, description = "Missing name or image, unknown type or supplier, negative price, size or units", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
        (status = 503, description = "Uploads are not configured", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<FixtureInput>,
) -> ApiResult<(StatusCode, Json<CreatedModel>)> {
    let id = fixtures::create(&state, fixtures::SINKS, user.company_id, &input).await?;
    Ok((StatusCode::CREATED, Json(CreatedModel { id })))
}

/// Edit a sink model
///
/// Saves the fields of a sink model of the caller's company. A new
/// `image_url` replaces the image (the old file is deleted). With `units`,
/// free units are added or removed to reach that total; units installed on
/// sales are never removed (400 when `units` is below them).
#[utoipa::path(
    put,
    path = "/v1/sinks/{id}",
    operation_id = "update_sink",
    tag = "sinks",
    params(("id" = i32, Path, description = "Sink model id")),
    request_body = FixtureInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Invalid field, or `units` below the installed units", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such model in the caller's company", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<FixtureInput>,
) -> ApiResult<StatusCode> {
    fixtures::update(&state, fixtures::SINKS, user.company_id, id, &input).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a sink model
///
/// Hides the model and its free units from every list. Units already
/// installed on sales stay on those sales.
#[utoipa::path(
    delete,
    path = "/v1/sinks/{id}",
    operation_id = "delete_sink",
    tag = "sinks",
    params(("id" = i32, Path, description = "Sink model id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such model in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    fixtures::delete(&state, fixtures::SINKS, user.company_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List a sink model's installed photos
///
/// Returns the project photos of the model, oldest first.
#[utoipa::path(
    get,
    path = "/v1/sinks/{id}/images",
    operation_id = "list_sink_images",
    tag = "sinks",
    params(("id" = i32, Path, description = "Sink model id")),
    responses(
        (status = 200, description = "Photos", body = Vec<FixturePhoto>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or of the shop", body = ErrorBody),
        (status = 404, description = "No such model in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn images(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<FixturePhoto>>> {
    Ok(Json(
        fixtures::photos(&state, fixtures::SINKS, user.company_id, id).await?,
    ))
}

/// Add installed photos to a sink model
///
/// Saves uploads to the `sinks` folder as project photos of the model and
/// stores their thumbnails.
#[utoipa::path(
    post,
    path = "/v1/sinks/{id}/images",
    operation_id = "add_sink_images",
    tag = "sinks",
    params(("id" = i32, Path, description = "Sink model id")),
    request_body = AddPhotos,
    responses(
        (status = 201, description = "Photos added", body = Vec<FixturePhoto>),
        (status = 400, description = "No URLs, more than 50, or a URL that is not an upload", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such model in the caller's company", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_images(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<AddPhotos>,
) -> ApiResult<(StatusCode, Json<Vec<FixturePhoto>>)> {
    let added =
        fixtures::add_photos(&state, fixtures::SINKS, user.company_id, id, body.urls).await?;
    Ok((StatusCode::CREATED, Json(added)))
}

/// Delete an installed photo of a sink model
///
/// Deletes the photo and its stored file.
#[utoipa::path(
    delete,
    path = "/v1/sinks/{id}/images/{image_id}",
    operation_id = "delete_sink_image",
    tag = "sinks",
    params(
        ("id" = i32, Path, description = "Sink model id"),
        ("image_id" = i32, Path, description = "Photo id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such photo on this model of the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_image(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path((id, image_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    fixtures::delete_photo(&state, fixtures::SINKS, user.company_id, id, image_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
