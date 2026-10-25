use super::queries::{edit, find_contract, sale_exists, sell, unsell as unsell_sale, validate};
use super::schemas::{CreatedSale, SaleContract, SaleInput};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

/// Get a sale for editing
///
/// Returns the sale's customer, seller, notes, price and its rooms, each with
/// the slabs used (`is_full` false when only part of a slab was sold), the
/// sinks and faucets installed, and the room options. 404 when the sale is
/// not the company's or no slab is left on it. Save changes with
/// `PUT /v1/sales/{id}`.
#[crate::openapi::path(
    get,
    path = "/v1/sales/{id}",
    operation_id = "get_sale",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id")),
    responses(
        (status = 200, description = "The sale", body = SaleContract),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such sale with slabs in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<SaleContract>> {
    find_contract(&state.pool, user.company_id, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// Sell slabs to a customer
///
/// Creates a pending sale dated now. Each room needs square feet and at least
/// one available slab of the company; a slab sold partially
/// (`is_full: false`) leaves a remnant in the yard, and a slab used in
/// several rooms gets a piece cut from it for each later room. Sinks and
/// faucets take one free unit of each model, installed on the room's first
/// slab. A customer without a sales rep gets the seller. All or nothing: any
/// error leaves the data unchanged.
#[crate::openapi::path(
    post,
    path = "/v1/sales",
    operation_id = "create_sale",
    tag = "sales",
    request_body = SaleInput,
    responses(
        (status = 201, description = "Sale created", body = CreatedSale),
        (status = 400, description = "Invalid body: no rooms, a room without slabs or square feet, unknown customer/slab/model, a slab already sold, or no free unit of a sink/faucet model", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<SaleInput>,
) -> ApiResult<(StatusCode, Json<CreatedSale>)> {
    let mut tx = state.pool.begin().await?;
    validate(&mut tx, user.company_id, &input, None).await?;
    let name = user.name.clone().unwrap_or_default();
    let id = sell(&mut tx, user.company_id, (user.id, &name), &input).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(CreatedSale { id })))
}

/// Save an edited sale
///
/// Replaces the sale's customer, notes, price, rooms, slabs, sinks and
/// faucets with the body (the seller only when `seller_id` is given). Slabs
/// may be ones the sale already has or available ones; slabs left out are
/// freed, with their sinks and faucets. All or nothing.
#[crate::openapi::path(
    put,
    path = "/v1/sales/{id}",
    operation_id = "update_sale",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id")),
    request_body = SaleInput,
    responses(
        (status = 204, description = "Sale saved"),
        (status = 400, description = "Invalid body (see `create_sale`)", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such sale in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SaleInput>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    if !sale_exists(&mut tx, user.company_id, id).await? {
        return Err(ApiError::NotFound);
    }
    validate(&mut tx, user.company_id, &input, Some(id)).await?;
    edit(&mut tx, user.company_id, id, &input).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Cancel a sale
///
/// Marks the sale cancelled, frees its slabs and the sinks and faucets on
/// them, and removes the free remnants its partial slabs left. Remnants that
/// another sale bought are kept. Returns no body.
#[crate::openapi::path(
    post,
    path = "/v1/sales/{id}/unsell",
    operation_id = "unsell_sale",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id")),
    responses(
        (status = 204, description = "Sale cancelled"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such sale in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn unsell(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    if !sale_exists(&mut tx, user.company_id, id).await? {
        return Err(ApiError::NotFound);
    }
    unsell_sale(&mut tx, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
