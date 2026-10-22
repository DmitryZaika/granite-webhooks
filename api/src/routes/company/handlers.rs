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

/// Query of `GET /v1/company/drive-miles`.
#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DriveMilesQuery {
    /// Where to drive to.
    #[serde(default)]
    pub address: String,
}

/// `GET /v1/company/drive-miles`.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct DriveMiles {
    /// Driving miles, one decimal; null when unknown.
    pub miles: Option<f64>,
}

/// Driving miles from the company
///
/// Miles by car from the company's address to `address`; null when either
/// address is missing or there is no route.
#[utoipa::path(
    get,
    path = "/v1/company/drive-miles",
    operation_id = "get_drive_miles",
    tag = "company",
    params(DriveMilesQuery),
    responses(
        (status = 200, description = "Miles", body = DriveMiles),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn drive_miles(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    axum::extract::Query(query): axum::extract::Query<DriveMilesQuery>,
) -> ApiResult<Json<DriveMiles>> {
    let address = query.address.trim();
    if address.is_empty() {
        return Ok(Json(DriveMiles { miles: None }));
    }
    let home: Option<Option<String>> =
        sqlx::query_scalar("SELECT address FROM company WHERE id = ?")
            .bind(user.company_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some(home) = home
        .flatten()
        .map(|home| home.trim().to_string())
        .filter(|h| !h.is_empty())
    else {
        return Ok(Json(DriveMiles { miles: None }));
    };
    Ok(Json(DriveMiles {
        miles: state.maps.driving_miles(&home, address).await,
    }))
}

/// `GET /v1/company/tax-rate`.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct TaxRate {
    /// The company's state sales tax in percent; 0 when not set.
    pub state_tax_pct: f64,
}

/// Get the company's sales tax
///
/// The state sales tax the company charges, for price calculators (0 when
/// not set).
#[utoipa::path(
    get,
    path = "/v1/company/tax-rate",
    operation_id = "get_company_tax_rate",
    tag = "company",
    responses(
        (status = 200, description = "The tax rate", body = TaxRate),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn tax_rate(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<TaxRate>> {
    let value: Option<Option<f64>> =
        sqlx::query_scalar("SELECT CAST(state_taxes AS DOUBLE) FROM company WHERE id = ?")
            .bind(user.company_id)
            .fetch_optional(&state.pool)
            .await?;
    let pct = value
        .flatten()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(0.0);
    Ok(Json(TaxRate { state_tax_pct: pct }))
}
