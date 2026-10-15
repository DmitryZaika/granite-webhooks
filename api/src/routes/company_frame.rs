//! `GET /v1/public/companies/{company_id}/frame`: a company's page frame.
//!
//! Its header (name and logo) and the stone, sink and faucet filters of its
//! catalog (types and suppliers). No session needed: the showroom pages
//! show the same.

use crate::error::{ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(company_frame))
}

/// A type or supplier of the catalog filters.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct NamedRow {
    /// Id.
    pub id: i64,
    /// Name.
    pub name: String,
}

/// A supplier of the catalog filters (`ISupplier`).
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct FrameSupplier {
    /// Supplier id.
    pub id: i32,
    /// Name.
    pub supplier_name: Option<String>,
}

/// The frame of a company's pages.
#[derive(Debug, Serialize, ToSchema)]
pub struct CompanyFrame {
    /// Company name.
    pub name: Option<String>,
    /// Logo.
    pub logo_url: Option<String>,
    /// Logo height in pixels (the app clamps it).
    pub logo_height: Option<i32>,
    /// Stone types, by name.
    pub stone_types: Vec<NamedRow>,
    /// Sink types, by name.
    pub sink_types: Vec<NamedRow>,
    /// Suppliers of its stones.
    pub stone_suppliers: Vec<FrameSupplier>,
    /// Suppliers of its sinks.
    pub sink_suppliers: Vec<FrameSupplier>,
    /// Suppliers of its faucets.
    pub faucet_suppliers: Vec<FrameSupplier>,
}

async fn suppliers(state: &AppState, company_id: i32, join: &str) -> ApiResult<Vec<FrameSupplier>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT s.id, s.supplier_name FROM suppliers s {join}
          WHERE s.company_id = ? GROUP BY s.id, s.supplier_name"
    )))
    .bind(company_id)
    .fetch_all(&state.pool)
    .await?)
}

/// Get a company's page frame
///
/// Its name and logo, and its catalog's stone and sink types and the
/// suppliers of its stones, sinks and faucets. Unknown companies get an
/// empty frame.
#[utoipa::path(
    get,
    path = "/v1/public/companies/{company_id}/frame",
    operation_id = "get_company_frame",
    tag = "public",
    params(("company_id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The frame", body = CompanyFrame),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn company_frame(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
) -> ApiResult<Json<CompanyFrame>> {
    let header: Option<(Option<String>, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT name, NULLIF(NULLIF(TRIM(logo_url), ''), 'undefined'), CAST(logo_height AS SIGNED)
           FROM company WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    let (name, logo_url, logo_height) = header.unwrap_or_default();
    let stone_types: Vec<NamedRow> =
        sqlx::query_as("SELECT CAST(id AS SIGNED) AS id, name FROM stone_types WHERE company_id = ? ORDER BY name ASC")
            .bind(company_id)
            .fetch_all(&state.pool)
            .await?;
    let sink_types: Vec<NamedRow> =
        sqlx::query_as("SELECT CAST(id AS SIGNED) AS id, name FROM sink_types WHERE company_id = ? ORDER BY id ASC")
            .bind(company_id)
            .fetch_all(&state.pool)
            .await?;
    Ok(Json(CompanyFrame {
        name,
        logo_url,
        logo_height: logo_height.and_then(|h| i32::try_from(h).ok()),
        stone_types,
        sink_types,
        stone_suppliers: suppliers(
            &state,
            company_id,
            "INNER JOIN stones st ON s.id = st.supplier_id",
        )
        .await?,
        sink_suppliers: suppliers(
            &state,
            company_id,
            "INNER JOIN sink_type sk ON s.id = sk.supplier_id",
        )
        .await?,
        faucet_suppliers: suppliers(
            &state,
            company_id,
            "INNER JOIN faucet_type ft ON s.id = ft.supplier_id",
        )
        .await?,
    }))
}
