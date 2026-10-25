//! `GET /v1/reports/cut-slabs`: whole slabs the shop cut, for the admin
//! Reports page.
//!
//! Replaces the Remix `admin.reports` loader.

use crate::auth::AdminUser;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::serde_helpers::js_date;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(cut_slabs))
}

/// Query of `GET /v1/reports/cut-slabs`. Every filter is optional.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CutSlabsQuery {
    /// Cut on or after this day, `YYYY-MM-DD`.
    pub from: Option<NaiveDate>,
    /// Cut on or before this day, `YYYY-MM-DD`.
    pub to: Option<NaiveDate>,
    /// Only stones of this supplier.
    pub supplier_id: Option<i32>,
    /// Only this stone.
    pub stone_id: Option<i32>,
}

/// A cut whole slab with its stone, supplier and sale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow, ToSchema)]
pub struct CutSlab {
    /// Slab id.
    pub id: i64,
    /// Bundle label.
    pub bundle: Option<String>,
    /// Stone id.
    pub stone_id: i32,
    /// Stone name.
    pub stone_name: Option<String>,
    /// Supplier id.
    pub supplier_id: i32,
    /// Supplier name.
    pub supplier_name: Option<String>,
    /// When it was cut.
    #[serde(serialize_with = "js_date")]
    pub cut_date: Option<DateTime<Utc>>,
    /// When it was sold; null when the sale is missing.
    #[serde(serialize_with = "js_date")]
    pub sale_date: Option<DateTime<Utc>>,
    /// Who sold it; null when unknown.
    pub seller_name: Option<String>,
}

/// Cut slabs report
///
/// Whole slabs (not remnants) of the caller's company that were cut, with
/// their stone, the stone's supplier and the sale, newest cut first. Slabs
/// of stones without a supplier are left out.
#[crate::openapi::path(
    get,
    path = "/v1/reports/cut-slabs",
    operation_id = "get_cut_slabs_report",
    tag = "reports",
    params(CutSlabsQuery),
    responses(
        (status = 200, description = "Cut slabs", body = Vec<CutSlab>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn cut_slabs(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Query(query): Query<CutSlabsQuery>,
) -> ApiResult<Json<Vec<CutSlab>>> {
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT CAST(si.id AS SIGNED) AS id, si.bundle, stones.id AS stone_id,
                stones.name AS stone_name, suppliers.id AS supplier_id,
                suppliers.supplier_name, si.cut_date, sales.sale_date,
                users.name AS seller_name
           FROM slab_inventory si
           JOIN stones ON si.stone_id = stones.id
           JOIN suppliers ON stones.supplier_id = suppliers.id
           LEFT JOIN sales ON si.sale_id = sales.id
           LEFT JOIN users ON sales.seller_id = users.id
          WHERE si.cut_date IS NOT NULL AND si.parent_id IS NULL
            AND stones.company_id = ",
    );
    sql.push_bind(user.company_id)
        .push(" AND suppliers.company_id = ")
        .push_bind(user.company_id);
    if let Some(from) = query.from {
        sql.push(" AND DATE(si.cut_date) >= ").push_bind(from);
    }
    if let Some(to) = query.to {
        sql.push(" AND DATE(si.cut_date) <= ").push_bind(to);
    }
    if let Some(supplier_id) = query.supplier_id {
        sql.push(" AND suppliers.id = ").push_bind(supplier_id);
    }
    if let Some(stone_id) = query.stone_id {
        sql.push(" AND stones.id = ").push_bind(stone_id);
    }
    sql.push(" ORDER BY si.cut_date DESC, si.id DESC");
    Ok(Json(sql.build_query_as().fetch_all(&state.pool).await?))
}
