use super::queries::{PositionRow, positions_in_company};
use crate::auth::CurrentUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;

/// Get the signed-in user
///
/// Returns the user behind the session, including their permission flags and
/// the company every other request acts in.
#[utoipa::path(
    get,
    path = "/v1/me",
    operation_id = "get_current_user",
    tag = "me",
    responses(
        (status = 200, description = "The signed-in user", body = SessionUser),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn me(CurrentUser(user): CurrentUser) -> Json<SessionUser> {
    Json(user)
}

/// List the signed-in user's positions
///
/// Returns the position ids (job roles such as sales rep) the user holds in
/// the company the request acts in.
#[utoipa::path(
    get,
    path = "/v1/me/positions",
    operation_id = "list_current_user_positions",
    tag = "me",
    responses(
        (status = 200, description = "Position ids in the current company", body = Vec<PositionRow>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn positions(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Vec<PositionRow>>> {
    let rows = positions_in_company(&state.pool, user.id, user.company_id).await?;
    Ok(Json(rows))
}
