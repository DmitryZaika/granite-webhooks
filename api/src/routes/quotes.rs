//! Quote tools.
//!
//! - `GET /v1/quotes` → the company's quotes for the quotes list (or one customer's)
//! - `DELETE /v1/quotes/{id}` → soft-deletes a quote
//! - `GET /v1/quotes/inventory-slabs` → a stone's free slabs for the layout
//! - `POST /v1/quotes/print-previews` → where to put a printed quote PDF to open it
//!
//! Replaces the Remix `employee.quotes` loader, the `employee.quotes.delete`
//! route, `api.quote-inventory-slabs` and `api.quote-preview`. Slab pictures for a
//! layout upload to the `quotes` folder (`POST /v1/uploads`).

use crate::auth::{EmployeeUser, ShopOrEmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::routes::quote_editor::STONE_IN_STOCK_SQL;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(
            list_quotes,
            crate::routes::quote_editor::create_quote
        ))
        .routes(routes!(
            delete_quote,
            crate::routes::quote_editor::get_quote,
            crate::routes::quote_editor::update_quote
        ))
        .routes(routes!(inventory_slabs))
        .routes(routes!(print_preview))
}

/// `positions.id` of a sales rep.
const SALES_REP: i32 = 1;

/// A quote in the quotes list.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct QuoteListRow {
    /// The quote.
    pub id: i32,
    /// Its name.
    pub quote_name: Option<String>,
    /// The customer's name.
    pub customer_name: Option<String>,
    /// Quote type.
    pub quote_type: String,
    /// `draft`, `sent`, `converted`, ...
    pub status: String,
    /// When it was made (UTC, ISO 8601).
    pub created_date: Option<String>,
    /// Last sent revision; 0 for a draft.
    pub current_revision: i32,
    /// Its price list.
    pub price_list_name: Option<String>,
    /// The salesperson's name.
    pub sales_rep: String,
    /// When the customer last opened an emailed revision (UTC, ISO 8601).
    pub email_viewed_at: Option<String>,
    /// Address, for the list search.
    pub address: Option<String>,
    /// Phone, for the list search.
    pub phone: Option<String>,
    /// Email, for the list search.
    pub email: Option<String>,
    /// The caller is its salesperson (or made it and it has none).
    pub mine: bool,
}

/// Query of `GET /v1/quotes`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct QuoteListQuery {
    /// Only this customer's quotes; every quote when absent.
    pub customer_id: Option<i64>,
}

/// `GET /v1/quotes`.
#[derive(Debug, Serialize, ToSchema)]
pub struct QuoteList {
    /// Not deleted quotes, newest first.
    pub quotes: Vec<QuoteListRow>,
    /// `all` for sales reps (they start on every quote), else `mine`.
    pub default_show: String,
}

/// List the company's quotes
///
/// Every quote of the caller's company that is not deleted (only one
/// customer's with `customer_id`), newest first, with its customer, price
/// list and when its email was last viewed.
#[utoipa::path(
    get,
    path = "/v1/quotes",
    operation_id = "list_quotes",
    tag = "quotes",
    params(QuoteListQuery),
    responses(
        (status = 200, description = "Quotes", body = QuoteList),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_quotes(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<QuoteListQuery>,
) -> ApiResult<Json<QuoteList>> {
    let quotes: Vec<QuoteListRow> = sqlx::query_as(
        "SELECT q.id, q.quote_name, c.name AS customer_name, q.quote_type, q.status,
                DATE_FORMAT(q.created_date, '%Y-%m-%dT%H:%i:%sZ') AS created_date,
                q.current_revision, pl.name AS price_list_name, q.sales_rep,
                q.address, q.phone, q.email,
                DATE_FORMAT(MAX(e.last_viewed_at), '%Y-%m-%dT%H:%i:%sZ') AS email_viewed_at,
                COALESCE(q.salesperson_id = ? OR (q.salesperson_id IS NULL AND q.created_by = ?), 0) = 1 AS mine
           FROM quotes q
           LEFT JOIN customers c ON c.id = q.customer_id
           LEFT JOIN price_lists pl ON pl.id = q.price_list_id
           LEFT JOIN quote_emails e ON e.quote_id = q.id
          WHERE q.company_id = ? AND q.deleted_at IS NULL
            AND (? IS NULL OR q.customer_id = ?)
          GROUP BY q.id
          ORDER BY q.created_date DESC, q.id DESC",
    )
    .bind(user.id)
    .bind(user.id)
    .bind(user.company_id)
    .bind(query.customer_id)
    .bind(query.customer_id)
    .fetch_all(&state.pool)
    .await?;
    let sales_rep: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM users_positions
          WHERE user_id = ? AND company_id = ? AND position_id = ? LIMIT 1",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(SALES_REP)
    .fetch_optional(&state.pool)
    .await?;
    let default_show = if sales_rep.is_some() { "all" } else { "mine" };
    Ok(Json(QuoteList {
        quotes,
        default_show: default_show.to_string(),
    }))
}

/// Delete a quote
///
/// Soft-deletes one of the company's quotes: it leaves the list, its rows stay.
#[utoipa::path(
    delete,
    path = "/v1/quotes/{id}",
    operation_id = "delete_quote",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 404, description = "No such quote in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_quote(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let done = sqlx::query(
        "UPDATE quotes SET deleted_at = NOW()
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
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
    /// A remnant, or what is left of a partly sold slab (its size and photo
    /// are the whole slab's).
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
/// layout. What is left of a partly sold slab is marked `leftover`.
/// `stone_url` is the photo of such a stone that is in stock (regular
/// stock or a free slab), null when none is.
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
                si.url, (COALESCE(si.is_leftover, 0) = 1 OR si.parent_id IS NOT NULL) AS leftover
           FROM slab_inventory si JOIN stones owner ON owner.id = si.stone_id
          WHERE owner.company_id = ? AND si.sale_id IS NULL AND si.cut_date IS NULL
            AND si.deleted_at IS NULL AND owner.deleted_at IS NULL
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
        "SELECT s.url FROM stones s WHERE s.id IN ({stones}) AND {STONE_IN_STOCK_SQL}
          ORDER BY COALESCE(s.regular_stock, 0) = 0, s.id LIMIT 1"
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

/// Body of `POST /v1/quotes/print-previews`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PrintPreviewInput {
    /// What the PDF is saved as, e.g. `Smith Kitchen.pdf` (ends with `.pdf`,
    /// at most 180 characters, none of `\ / : * ? " < > |`).
    pub filename: String,
}

/// Where the printed PDF goes, and where to open it.
#[derive(Debug, Serialize, ToSchema)]
pub struct PrintPreview {
    /// `PUT` the PDF here (valid 15 minutes) with `Content-Type: application/pdf`.
    pub upload_url: String,
    /// Open this (valid an hour) to show the PDF; the viewer saves it under
    /// the file name.
    pub url: String,
}

fn valid_print_name(name: &str) -> Option<&str> {
    let name = name.trim();
    let bad = |c: char| {
        matches!(
            c,
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\r' | '\n'
        )
    };
    (name.to_lowercase().ends_with(".pdf") && name.chars().count() <= 180 && !name.contains(bad))
        .then_some(name)
}

/// Open a printed quote
///
/// The quote page draws the PDF in the browser; this gives it a place in the
/// bucket (`quote-previews/`) and a link to open it under its file name in
/// the print window. Replaces the CRM's in-memory preview (10 minutes);
/// the bucket's lifecycle rule removes old previews.
#[utoipa::path(
    post,
    path = "/v1/quotes/print-previews",
    operation_id = "create_quote_print_preview",
    tag = "quotes",
    request_body = PrintPreviewInput,
    responses(
        (status = 201, description = "Upload and open links", body = PrintPreview),
        (status = 400, description = "Not a PDF file name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 503, description = "No bucket configured", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn print_preview(
    State(state): State<AppState>,
    EmployeeUser(_user): EmployeeUser,
    crate::extract::JsonBody(input): crate::extract::JsonBody<PrintPreviewInput>,
) -> ApiResult<(StatusCode, Json<PrintPreview>)> {
    let filename = valid_print_name(&input.filename)
        .ok_or_else(|| ApiError::BadRequest("Invalid preview".into()))?;
    let key = format!("quote-previews/{}.pdf", uuid::Uuid::new_v4());
    let upload_url = state.storage.presign_put(&key, "application/pdf").await?;
    let url = state
        .storage
        .presign_get_as(&key, "application/pdf", Some(filename))
        .await?;
    Ok((StatusCode::CREATED, Json(PrintPreview { upload_url, url })))
}

#[cfg(test)]
mod print_tests {
    use super::valid_print_name;

    #[test]
    fn print_names_are_plain_pdf_names() {
        assert_eq!(
            valid_print_name(" Smith Kitchen.pdf "),
            Some("Smith Kitchen.pdf")
        );
        assert_eq!(valid_print_name("Café.PDF"), Some("Café.PDF"));
        assert_eq!(valid_print_name("a/b.pdf"), None);
        assert_eq!(valid_print_name("quote.txt"), None);
        assert_eq!(valid_print_name(&format!("{}.pdf", "x".repeat(180))), None);
    }
}
