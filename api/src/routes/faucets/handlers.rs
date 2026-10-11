use super::queries::list_faucets;
use super::schemas::{Faucet, ListFaucetsQuery};
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::routes::fixtures::{
    self, AddPhotos, CreatedModel, FixtureInput, FixtureModel, FixturePhoto,
};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

/// List the company's faucet models with free units
///
/// Returns the non-deleted faucet models of the caller's company, sorted by
/// name, each with the number of units free to sell (`available`). Models
/// not on display are included (`is_display`). Sold out models (no free unit
/// and not regular stock) are hidden unless `include_sold_out=true`. Filters
/// combine with AND; `type` takes the parameter repeated and matches any value.
#[utoipa::path(
    get,
    path = "/v1/faucets",
    operation_id = "list_faucets",
    tag = "faucets",
    params(ListFaucetsQuery),
    responses(
        (status = 200, description = "Faucet models sorted by name", body = Vec<Faucet>),
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
    MultiQuery(query): MultiQuery<ListFaucetsQuery>,
) -> ApiResult<Json<Vec<Faucet>>> {
    Ok(Json(
        list_faucets(&state.pool, user.company_id, &query).await?,
    ))
}

// ---- Model detail and admin writes (shared code in `routes::fixtures`)

/// Get one faucet model
///
/// Returns a live faucet model of the caller's company with its unit counts
/// (`units` includes the `installed_units` on sales).
#[utoipa::path(
    get,
    path = "/v1/faucets/{id}",
    operation_id = "get_faucet",
    tag = "faucets",
    params(("id" = i32, Path, description = "Faucet model id")),
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
    fixtures::find(&mut conn, fixtures::FAUCETS, user.company_id, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// Add a faucet model
///
/// Creates a faucet model of the caller's company with its image (an upload
/// to the `faucets` folder; the thumbnail is made here) and, with `units`,
/// that many units in stock.
#[utoipa::path(
    post,
    path = "/v1/faucets",
    operation_id = "create_faucet",
    tag = "faucets",
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
    let id = fixtures::create(&state, fixtures::FAUCETS, user.company_id, &input).await?;
    Ok((StatusCode::CREATED, Json(CreatedModel { id })))
}

/// Edit a faucet model
///
/// Saves the fields of a faucet model of the caller's company. A new
/// `image_url` replaces the image (the old file is deleted). With `units`,
/// free units are added or removed to reach that total; units installed on
/// sales are never removed (400 when `units` is below them).
#[utoipa::path(
    put,
    path = "/v1/faucets/{id}",
    operation_id = "update_faucet",
    tag = "faucets",
    params(("id" = i32, Path, description = "Faucet model id")),
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
    fixtures::update(&state, fixtures::FAUCETS, user.company_id, id, &input).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a faucet model
///
/// Hides the model and its free units from every list. Units already
/// installed on sales stay on those sales.
#[utoipa::path(
    delete,
    path = "/v1/faucets/{id}",
    operation_id = "delete_faucet",
    tag = "faucets",
    params(("id" = i32, Path, description = "Faucet model id")),
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
    fixtures::delete(&state, fixtures::FAUCETS, user.company_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List a faucet model's installed photos
///
/// Returns the project photos of the model, oldest first.
#[utoipa::path(
    get,
    path = "/v1/faucets/{id}/images",
    operation_id = "list_faucet_images",
    tag = "faucets",
    params(("id" = i32, Path, description = "Faucet model id")),
    responses(
        (status = 200, description = "Photos", body = Vec<FixturePhoto>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such model in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn images(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<FixturePhoto>>> {
    Ok(Json(
        fixtures::photos(&state, fixtures::FAUCETS, user.company_id, id).await?,
    ))
}

/// Add installed photos to a faucet model
///
/// Saves uploads to the `faucets` folder as project photos of the model and
/// stores their thumbnails.
#[utoipa::path(
    post,
    path = "/v1/faucets/{id}/images",
    operation_id = "add_faucet_images",
    tag = "faucets",
    params(("id" = i32, Path, description = "Faucet model id")),
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
        fixtures::add_photos(&state, fixtures::FAUCETS, user.company_id, id, body.urls).await?;
    Ok((StatusCode::CREATED, Json(added)))
}

/// Delete an installed photo of a faucet model
///
/// Deletes the photo and its stored file.
#[utoipa::path(
    delete,
    path = "/v1/faucets/{id}/images/{image_id}",
    operation_id = "delete_faucet_image",
    tag = "faucets",
    params(
        ("id" = i32, Path, description = "Faucet model id"),
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
    fixtures::delete_photo(&state, fixtures::FAUCETS, user.company_id, id, image_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
