use super::queries::{PositionRow, positions_in_company};
use crate::auth::CurrentUser;
use crate::auth::session::SessionUser;
use crate::error::ApiResult;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;

pub async fn me(CurrentUser(user): CurrentUser) -> Json<SessionUser> {
    Json(user)
}

pub async fn positions(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Vec<PositionRow>>> {
    let rows = positions_in_company(&state.pool, user.id, user.company_id).await?;
    Ok(Json(rows))
}
