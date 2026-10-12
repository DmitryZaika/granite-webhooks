//! Quote tools.
//!
//! - `GET /v1/quotes/inventory-slabs` → a stone's free slabs for the layout
//!
//! Replaces the Remix `api.quote-inventory-slabs` route. Slab pictures for a
//! layout upload to the `quotes` folder (`POST /v1/uploads`).

use crate::auth::ShopOrEmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(inventory_slabs))
}

/// Query of `GET /v1/quotes/inventory-slabs`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct InventorySlabsQuery {
    /// Stone name (any case).
    #[serde(default)]
    pub name: String,
    /// Stone type (any case); any type when empty.
    #[serde(default, rename = "type")]
    #[param(rename = "type")]
    pub kind: String,
}

/// A free slab.
#[derive(Debug, Serialize, ToSchema)]
pub struct InventorySlab {
    /// The slab.
    pub id: i64,
    /// Bundle label.
    pub bundle: Option<String>,
    /// Width in inches.
    pub width: Option<f64>,
    /// Length in inches.
    pub length: Option<f64>,
    /// Slab photo.
    pub url: Option<String>,
    /// A remnant.
    pub leftover: bool,
}

/// `GET /v1/quotes/inventory-slabs`.
#[derive(Debug, Serialize, ToSchema)]
pub struct InventorySlabs {
    /// The stone's picture.
    pub stone_url: Option<String>,
    /// Free slabs of the stone and of the stones it shows slabs from, by bundle.
    pub slabs: Vec<InventorySlab>,
}

#[derive(sqlx::FromRow)]
struct SlabRow {
    id: i64,
    bundle: Option<String>,
    width: Option<f64>,
    length: Option<f64>,
    url: Option<String>,
    leftover: bool,
}

/// A real picture URL (not blank, `undefined` or `null`).
fn picture(url: Option<String>) -> Option<String> {
    url.map(|url| url.trim().to_string()).filter(|url| {
        !url.is_empty()
            && !url.eq_ignore_ascii_case("undefined")
            && !url.eq_ignore_ascii_case("null")
    })
}

/// List a stone's free slabs
///
/// Slabs not sold, cut or deleted of the company's stones with this name
/// (and type), and of the stones they show slabs from, for a quote's slab
/// layout.
#[utoipa::path(
    get,
    path = "/v1/quotes/inventory-slabs",
    operation_id = "list_quote_inventory_slabs",
    tag = "quotes",
    params(InventorySlabsQuery),
    responses(
        (status = 200, description = "Slabs", body = InventorySlabs),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or shop staff", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn inventory_slabs(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Query(query): Query<InventorySlabsQuery>,
) -> ApiResult<Json<InventorySlabs>> {
    let name = query.name.trim().to_lowercase();
    if name.is_empty() {
        return Ok(Json(InventorySlabs {
            stone_url: None,
            slabs: Vec::new(),
        }));
    }
    let kind = query.kind.trim().to_lowercase();
    // `?` is the type filter; an empty type matches every stone.
    let stones = "SELECT s.id FROM stones s
                   WHERE s.company_id = ? AND s.deleted_at IS NULL AND LOWER(TRIM(s.name)) = ?
                     AND (? = '' OR LOWER(TRIM(s.type)) = ?)";
    let slabs: Vec<SlabRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(si.id AS SIGNED) AS id, NULLIF(TRIM(si.bundle), '') AS bundle,
                CASE WHEN si.width > 0 THEN CAST(si.width AS DOUBLE) END AS width,
                CASE WHEN si.length > 0 THEN CAST(si.length AS DOUBLE) END AS length,
                si.url, COALESCE(si.is_leftover, 0) = 1 AS leftover
           FROM slab_inventory si JOIN stones owner ON owner.id = si.stone_id
          WHERE owner.company_id = ? AND si.sale_id IS NULL AND si.cut_date IS NULL
            AND si.deleted_at IS NULL
            AND (si.stone_id IN ({stones})
                 OR si.stone_id IN (SELECT l.source_stone_id FROM stone_slab_links l
                                     WHERE l.stone_id IN ({stones})))
          ORDER BY si.bundle, si.id"
    )))
    .bind(user.company_id)
    .bind(user.company_id)
    .bind(&name)
    .bind(&kind)
    .bind(&kind)
    .bind(user.company_id)
    .bind(&name)
    .bind(&kind)
    .bind(&kind)
    .fetch_all(&state.pool)
    .await?;
    let stone_url: Option<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT s.url FROM stones s WHERE s.id IN ({stones}) ORDER BY s.id LIMIT 1"
    )))
    .bind(user.company_id)
    .bind(&name)
    .bind(&kind)
    .bind(&kind)
    .fetch_optional(&state.pool)
    .await?;
    Ok(Json(InventorySlabs {
        stone_url: picture(stone_url.flatten()),
        slabs: slabs
            .into_iter()
            .map(|row| InventorySlab {
                id: row.id,
                bundle: row.bundle,
                width: row.width,
                length: row.length,
                url: picture(row.url),
                leftover: row.leftover,
            })
            .collect(),
    }))
}
