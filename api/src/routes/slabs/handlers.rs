use super::queries::update_slab_size;
use super::schemas::UpdateSlab;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;

/// Change a slab's size
///
/// Sets the length and width (inches) of one slab or remnant of the
/// caller's company. Both must be positive numbers. Returns no body; reload
/// the stone's slabs (`GET /v1/stones/{id}/slabs`) to see the change.
#[utoipa::path(
    patch,
    path = "/v1/slabs/{id}",
    operation_id = "update_slab_size",
    tag = "slabs",
    params(("id" = i64, Path, description = "Slab id")),
    request_body = UpdateSlab,
    responses(
        (status = 204, description = "Size saved"),
        (status = 400, description = "Missing, non-numeric or non-positive size", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such slab in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
    JsonBody(body): JsonBody<UpdateSlab>,
) -> ApiResult<StatusCode> {
    for (name, value) in [("length", body.length), ("width", body.width)] {
        if !value.is_finite() || value <= 0.0 {
            return Err(ApiError::BadRequest(format!(
                "{name} must be a positive number"
            )));
        }
    }
    if update_slab_size(&state.pool, user.company_id, id, body.length, body.width).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}
