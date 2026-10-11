use super::queries::company_features;
use super::schemas::CompanyFeatures;
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;

/// Get the company's features
///
/// Returns which features the company the request acts in has switched on
/// (QuickBooks, guided tours, the points leaderboard) and which integrations
/// it has set up (QuickBooks connection, e-signatures, inbound email,
/// CloudTalk, RingCentral). Use it to decide what to offer before calling the
/// routes behind those features.
#[utoipa::path(
    get,
    path = "/v1/company/features",
    operation_id = "get_company_features",
    tag = "company",
    responses(
        (status = 200, description = "The company's features", body = CompanyFeatures),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn features(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<CompanyFeatures>> {
    Ok(Json(company_features(&state.pool, user.company_id).await?))
}
