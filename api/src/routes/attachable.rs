//! What can be attached to a message: stones with their photos, the
//! company's image library and documents.
//!
//! - `GET /v1/stones/attachable` → displayed stones with an image and stock
//! - `GET /v1/images`            → the image library
//! - `GET /v1/documents`         → the documents
//!
//! Replaces the Remix `api.{stones,images,documents}.list.$companyId` routes.

use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::stones::queries::escape_like;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(stones))
        .routes(routes!(images))
        .routes(routes!(documents))
}

/// Query of `GET /v1/stones/attachable`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AttachableStonesQuery {
    /// Part of the name.
    #[serde(default)]
    pub name: String,
    /// Only these types (repeat the parameter).
    #[serde(default, rename = "type")]
    #[param(rename = "type")]
    pub types: Vec<String>,
    /// Only these finishes (repeat the parameter).
    #[serde(default)]
    pub finishing: Vec<String>,
    /// Only these price levels (repeat the parameter).
    #[serde(default)]
    pub level: Vec<i32>,
}

/// A stone to attach.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct AttachableStone {
    /// The stone.
    pub id: i32,
    /// Its name.
    pub name: Option<String>,
    /// The stone's image.
    pub url: Option<String>,
    #[serde(rename = "type")]
    #[sqlx(rename = "type")]
    #[schema(rename = "type")]
    /// Material type, e.g. `granite`.
    pub kind: Option<String>,
    /// `polished`, `leathered` or `honed`.
    pub finishing: Option<String>,
    /// Price level, 1 (cheapest) to 7.
    pub level: Option<i32>,
    /// Installed (project) photos, its own and those it shows from other stones.
    pub installed_count: i64,
}

/// Query of the library lists.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct NameQuery {
    /// Part of the name.
    #[serde(default)]
    pub name: String,
}

/// An image or document of the company.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct LibraryFile {
    /// The file.
    pub id: i32,
    /// Its name.
    pub name: Option<String>,
    /// Where it is stored.
    pub url: Option<String>,
}

fn push_list(sql: &mut QueryBuilder<MySql>, column: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    sql.push(format!(" AND {column} IN ("));
    let mut list = sql.separated(", ");
    for value in values {
        list.push_bind(value.clone());
    }
    sql.push(")");
}

/// List stones to attach
///
/// Displayed stones of the company with an image that are regular stock or
/// have a free slab, by name (at most 200), with their installed photos'
/// count.
#[crate::openapi::path(
    get,
    path = "/v1/stones/attachable",
    operation_id = "list_attachable_stones",
    tag = "stones",
    params(AttachableStonesQuery),
    responses(
        (status = 200, description = "Stones by name", body = Vec<AttachableStone>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn stones(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<AttachableStonesQuery>,
) -> ApiResult<Json<Vec<AttachableStone>>> {
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT s.id, s.name, s.url, s.type, s.finishing, s.level,
                (SELECT COUNT(*) FROM installed_stones own WHERE own.stone_id = s.id)
                + (SELECT COUNT(*) FROM stone_image_links sil
                     JOIN installed_stones linked ON linked.stone_id = sil.source_stone_id
                    WHERE sil.stone_id = s.id) AS installed_count
           FROM stones s
           LEFT JOIN (SELECT id AS stone_id, id AS source_stone_id FROM stones WHERE company_id = ",
    );
    sql.push_bind(user.company_id)
        .push(
            " UNION SELECT stone_id, source_stone_id FROM stone_slab_links) slab_sources
                ON slab_sources.stone_id = s.id
           LEFT JOIN slab_inventory si ON si.stone_id = slab_sources.source_stone_id
          WHERE s.company_id = ",
        )
        .push_bind(user.company_id)
        .push(
            " AND s.deleted_at IS NULL AND s.is_display = 1 AND s.url IS NOT NULL AND s.url != ''",
        );
    let name = query.name.trim();
    if !name.is_empty() {
        sql.push(" AND s.name LIKE ")
            .push_bind(format!("%{}%", escape_like(name)));
    }
    push_list(&mut sql, "s.type", &query.types);
    push_list(&mut sql, "s.finishing", &query.finishing);
    if !query.level.is_empty() {
        sql.push(" AND s.level IN (");
        let mut list = sql.separated(", ");
        for level in &query.level {
            list.push_bind(*level);
        }
        sql.push(")");
    }
    sql.push(
        " GROUP BY s.id
          HAVING SUM(si.id IS NOT NULL AND si.sale_id IS NULL AND si.cut_date IS NULL
                     AND si.deleted_at IS NULL) > 0 OR MAX(s.regular_stock) = 1
          ORDER BY s.name, s.id LIMIT 200",
    );
    Ok(Json(sql.build_query_as().fetch_all(&state.pool).await?))
}

async fn library(
    state: &AppState,
    company_id: i32,
    table: &str,
    name: &str,
) -> Result<Vec<LibraryFile>, sqlx::Error> {
    let mut sql = QueryBuilder::<MySql>::new(format!(
        "SELECT id, name, url FROM {table} WHERE company_id = "
    ));
    sql.push_bind(company_id);
    let name = name.trim();
    if !name.is_empty() {
        sql.push(" AND name LIKE ")
            .push_bind(format!("%{}%", escape_like(name)));
    }
    // A search fills a picker; the whole library is a page of its own.
    sql.push(if name.is_empty() {
        " ORDER BY name, id LIMIT 2000"
    } else {
        " ORDER BY name, id LIMIT 200"
    });
    sql.build_query_as().fetch_all(&state.pool).await
}

/// List the image library
///
/// The company's images by name (at most 200 matching `name`, else the
/// whole library).
#[crate::openapi::path(
    get,
    path = "/v1/images",
    operation_id = "list_images",
    tag = "catalog",
    params(NameQuery),
    responses(
        (status = 200, description = "Images by name", body = Vec<LibraryFile>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn images(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<NameQuery>,
) -> ApiResult<Json<Vec<LibraryFile>>> {
    Ok(Json(
        library(&state, user.company_id, "images", &query.name).await?,
    ))
}

/// List the documents
///
/// The company's documents by name (at most 200 matching `name`, else the
/// whole library).
#[crate::openapi::path(
    get,
    path = "/v1/documents",
    operation_id = "list_documents",
    tag = "catalog",
    params(NameQuery),
    responses(
        (status = 200, description = "Documents by name", body = Vec<LibraryFile>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn documents(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<NameQuery>,
) -> ApiResult<Json<Vec<LibraryFile>>> {
    Ok(Json(
        library(&state, user.company_id, "documents", &query.name).await?,
    ))
}
