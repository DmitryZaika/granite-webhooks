//! Feature sales: the extras (waterfalls, miters, phantom seams, …) sold on
//! each sold quote, priced from the quote itself (no Commissions sync).
//!
//! - `GET /v1/feature-sales?scope=mine|company&from=&to=` → sold quotes' extras

use crate::auth::EmployeeUser;
use crate::commissions::feature_sales;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::quote_pricing::pricing::price_quote;
use crate::quote_pricing::{load_company_quote, price_list_for_quote};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(list_feature_sales))
}

/// `?scope=&from=&to=`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FeatureSalesQuery {
    /// `mine` (default) or `company` (admins).
    #[serde(default)]
    pub scope: Option<String>,
    /// First sold day (YYYY-MM-DD); none: no limit.
    #[serde(default)]
    pub from: Option<String>,
    /// Last sold day (YYYY-MM-DD); none: no limit.
    #[serde(default)]
    pub to: Option<String>,
}

/// One sold quote's extras.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeatureSale {
    /// The quote.
    pub quote_id: i32,
    /// The rep: its salesperson, else who made it.
    pub user_id: Option<i32>,
    /// Their name ("Unassigned" when none).
    pub rep: String,
    /// When it was sold or signed (YYYY-MM-DD).
    pub signed_at: Option<String>,
    /// Dollars of each extra on the accepted option (`waterfall`, `miter`,
    /// `phantom_seam`, `custom_edge`, `support`, `faucet`, `soap_dispenser`).
    pub features: BTreeMap<String, f64>,
}

/// Sold quotes' extras.
#[derive(Debug, Serialize, ToSchema)]
pub struct FeatureSales {
    /// Sold quotes with at least one extra, newest first.
    pub rows: Vec<FeatureSale>,
}

#[derive(sqlx::FromRow)]
struct SoldQuote {
    id: i32,
    user_id: Option<i32>,
    rep: Option<String>,
    signed_at: Option<String>,
}

fn day(value: Option<&str>) -> ApiResult<Option<String>> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    chrono::NaiveDate::parse_from_str(value.get(..10).unwrap_or(value), "%Y-%m-%d")
        .map(|d| Some(d.format("%Y-%m-%d").to_string()))
        .map_err(|_| ApiError::BadRequest("from and to must be YYYY-MM-DD".into()))
}

/// List feature sales
///
/// The caller's (`mine`) or, for admins, everyone's (`company`) sold quotes
/// in the dates, each with the dollars of every extra on its accepted option,
/// worked out from the quote as it is now.
#[crate::openapi::path(
    get,
    path = "/v1/feature-sales",
    operation_id = "list_feature_sales",
    tag = "quotes",
    params(FeatureSalesQuery),
    responses(
        (status = 200, description = "Sold quotes' extras", body = FeatureSales),
        (status = 400, description = "Unknown scope or bad date", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, or not an admin for `company`", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_feature_sales(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<FeatureSalesQuery>,
) -> ApiResult<Json<FeatureSales>> {
    let only = match query.scope.as_deref() {
        None | Some("mine") => Some(user.id),
        Some("company") if user.is_admin || user.is_superuser => None,
        Some("company") => return Err(ApiError::Forbidden),
        Some(_) => return Err(ApiError::BadRequest("scope must be mine or company".into())),
    };
    let from = day(query.from.as_deref())?;
    let to = day(query.to.as_deref())?;
    // Sold: signed by the customer, or marked sold; the day as the
    // Commissions sync dates it.
    let quotes: Vec<SoldQuote> = sqlx::query_as(
        "SELECT s.id, s.user_id, u.name AS rep, s.signed_at
           FROM (SELECT q.id, COALESCE(q.salesperson_id, q.created_by) AS user_id,
                        DATE_FORMAT(COALESCE(q.signed_at,
                                             (SELECT MIN(e.created_date) FROM events e WHERE e.quote_id = q.id),
                                             q.updated_at, q.created_date),
                                    '%Y-%m-%d') AS signed_at
                   FROM quotes q
                  WHERE q.company_id = ? AND q.deleted_at IS NULL
                    AND (q.signed_at IS NOT NULL OR q.status IN ('converted', 'accepted'))) s
           LEFT JOIN users u ON u.id = s.user_id
          WHERE (? IS NULL OR s.user_id = ?)
            AND (? IS NULL OR s.signed_at >= ?)
            AND (? IS NULL OR s.signed_at <= ?)
          ORDER BY s.signed_at DESC, s.id DESC",
    )
    .bind(user.company_id)
    .bind(only)
    .bind(only)
    .bind(from.as_deref())
    .bind(from.as_deref())
    .bind(to.as_deref())
    .bind(to.as_deref())
    .fetch_all(&state.pool)
    .await?;
    let mut rows = Vec::new();
    for sold in quotes {
        let Some(quote) = load_company_quote(&state.pool, sold.id, user.company_id).await? else {
            continue;
        };
        let doc = quote.doc();
        let price_list = price_list_for_quote(&state.pool, &quote).await?;
        let prices = price_quote(&doc, &price_list, quote.options());
        let accepted = usize::try_from(quote.customer_accepted_option_id.unwrap_or(0).max(0))
            .unwrap_or_default()
            .min(prices.len().saturating_sub(1));
        let features = prices.get(accepted).map(feature_sales).unwrap_or_default();
        if features.is_empty() {
            continue;
        }
        rows.push(FeatureSale {
            quote_id: sold.id,
            user_id: sold.user_id,
            rep: sold.rep.unwrap_or_else(|| "Unassigned".into()),
            signed_at: sold.signed_at,
            features,
        });
    }
    Ok(Json(FeatureSales { rows }))
}
