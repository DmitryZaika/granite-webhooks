//! `GET /v1/shop/cut-stats`: square feet the shop finished cutting per day
//! and per material, for the shop Statistics page.
//!
//! Replaces the Remix `shop.statistics` loader.

use crate::auth::ShopOrEmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use chrono::{Days, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(cut_stats))
}

/// Query of `GET /v1/shop/cut-stats`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CutStatsQuery {
    /// First day, `YYYY-MM-DD`; default 29 days before `end`.
    pub start: Option<NaiveDate>,
    /// Last day, `YYYY-MM-DD`; default today (UTC).
    pub end: Option<NaiveDate>,
}

/// Square feet of one material.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct MaterialSqft {
    /// Stone type, e.g. `granite`; `Unknown` when the stone has none.
    pub stone_type: String,
    /// Square feet.
    pub sqft: f64,
}

/// One day with cutting finished.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct DayCut {
    /// The day, `YYYY-MM-DD`.
    pub day: String,
    /// Square feet that day.
    pub total: f64,
    /// By material, most first.
    pub materials: Vec<MaterialSqft>,
}

/// Cutting over a range of days.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct CutStats {
    /// The range used, `YYYY-MM-DD`.
    pub start: String,
    /// The range used, `YYYY-MM-DD`.
    pub end: String,
    /// Days with cutting, oldest first.
    pub days: Vec<DayCut>,
    /// The whole range by material, most first.
    pub materials: Vec<MaterialSqft>,
    /// Square feet over the range.
    pub total_sqft: f64,
}

fn by_most(map: BTreeMap<String, f64>) -> Vec<MaterialSqft> {
    let mut list: Vec<MaterialSqft> = map
        .into_iter()
        .map(|(stone_type, sqft)| MaterialSqft { stone_type, sqft })
        .collect();
    list.sort_by(|a, b| b.sqft.total_cmp(&a.sqft));
    list
}

/// Cut square feet per day
///
/// Square feet of sold slabs whose room is fully cut, by the day each slab
/// was cut and by stone type, for the caller's company. A slab's square
/// feet are its `square_feet`, else length × width / 144. Rooms with any
/// uncut slab are left out.
#[crate::openapi::path(
    get,
    path = "/v1/shop/cut-stats",
    operation_id = "get_shop_cut_stats",
    tag = "shop",
    params(CutStatsQuery),
    responses(
        (status = 200, description = "Cutting by day and material", body = CutStats),
        (status = 400, description = "`start` after `end`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not of the shop or an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn cut_stats(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Query(query): Query<CutStatsQuery>,
) -> ApiResult<Json<CutStats>> {
    let end = query.end.unwrap_or_else(|| Utc::now().date_naive());
    let start = query
        .start
        .unwrap_or_else(|| end.checked_sub_days(Days::new(29)).unwrap_or(end));
    if start > end {
        return Err(ApiError::BadRequest("start must not be after end".into()));
    }
    let rows: Vec<(NaiveDate, String, f64)> = sqlx::query_as(
        "SELECT DATE(si.cut_date), COALESCE(st.type, 'Unknown'),
                CAST(COALESCE(si.square_feet, (si.length * si.width) / 144, 0) AS DOUBLE)
           FROM slab_inventory si
           JOIN sales s ON si.sale_id = s.id
           JOIN stones st ON si.stone_id = st.id
          WHERE s.company_id = ? AND si.deleted_at IS NULL AND si.room IS NOT NULL
            AND si.cut_date IS NOT NULL AND DATE(si.cut_date) BETWEEN ? AND ?
            AND NOT EXISTS (
              SELECT 1 FROM slab_inventory si2
               WHERE si2.sale_id = si.sale_id AND si2.room = si.room
                 AND si2.deleted_at IS NULL AND si2.cut_date IS NULL)",
    )
    .bind(user.company_id)
    .bind(start)
    .bind(end)
    .fetch_all(&state.pool)
    .await?;
    let mut days: BTreeMap<NaiveDate, BTreeMap<String, f64>> = BTreeMap::new();
    let mut materials: BTreeMap<String, f64> = BTreeMap::new();
    for (day, stone_type, sqft) in rows {
        *days
            .entry(day)
            .or_default()
            .entry(stone_type.clone())
            .or_default() += sqft;
        *materials.entry(stone_type).or_default() += sqft;
    }
    let days: Vec<DayCut> = days
        .into_iter()
        .map(|(day, by_type)| {
            let materials = by_most(by_type);
            DayCut {
                day: day.to_string(),
                total: materials.iter().fold(0.0, |sum, m| sum + m.sqft),
                materials,
            }
        })
        .collect();
    Ok(Json(CutStats {
        start: start.to_string(),
        end: end.to_string(),
        total_sqft: days.iter().fold(0.0, |sum, d| sum + d.total),
        days,
        materials: by_most(materials),
    }))
}
