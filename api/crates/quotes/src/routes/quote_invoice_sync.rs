//! A quote's invoice kept in step with the quote.
//!
//! Creating an invoice from a quote with "Keep in sync with the quote" ticked
//! (Admin › Quotes › Automation "Automatically synchronize quote with
//! invoices" ticks it by default) links the invoice to the quote here. The
//! quote page works out the invoice from the quote; when the quote's invoice
//! lines no longer match `source_hash`, it rebuilds the invoice through the
//! ordinary invoice endpoints: on its own while every invoice is unpaid and
//! unsent, after asking otherwise. Then it saves the new hash here.
//!
//! - `GET /v1/quotes/{id}/invoice-sync`
//! - `PUT /v1/quotes/{id}/invoice-sync`

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_invoice_sync, save_invoice_sync))
}

/// Longest kept `detail`, in bytes of JSON.
const MAX_DETAIL: usize = 60_000;

/// At most this many invoices (a split of 12).
const MAX_INVOICES: usize = 12;

/// A quote's invoice and how it is kept in step with the quote.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct InvoiceSync {
    /// The quote option invoiced.
    pub option_index: i32,
    /// `qbo` (`QuickBooks` invoices) or `own` (`/v1/invoices`).
    pub kind: String,
    /// `QuickBooks` invoice ids in order (several for a split), or the own
    /// invoice's id.
    pub invoice_ids: Vec<String>,
    /// What rebuilding needs, as the CRM sent it (a split's shares, each
    /// invoice's priced lines by part, what was emailed).
    #[schema(value_type = Object)]
    pub detail: Value,
    /// The quote's invoice lines when the invoice was last made or synced.
    pub source_hash: String,
    /// Off: the invoice is no longer changed with the quote.
    pub enabled: bool,
}

#[derive(FromRow)]
struct SyncRow {
    option_index: i32,
    kind: String,
    invoice_ids: String,
    detail_json: String,
    source_hash: String,
    enabled: bool,
}

async fn quote_exists(state: &AppState, company_id: i32, quote_id: i32) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM quotes WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(quote_id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// Get a quote's invoice sync
///
/// The quote's invoice kept in step with it; null when it has none.
#[crate::openapi::path(
    get,
    path = "/v1/quotes/{id}/invoice-sync",
    operation_id = "get_quote_invoice_sync",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    responses(
        (status = 200, description = "The link, or null", body = Option<InvoiceSync>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_invoice_sync(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(quote_id): Path<i32>,
) -> ApiResult<Json<Option<InvoiceSync>>> {
    quote_exists(&state, user.company_id, quote_id).await?;
    let row: Option<SyncRow> = sqlx::query_as(
        "SELECT option_index, kind, invoice_ids, CAST(detail_json AS CHAR) AS detail_json,
                source_hash, enabled <> 0 AS enabled
           FROM quote_invoice_sync
          WHERE quote_id = ? AND company_id = ?",
    )
    .bind(quote_id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(Json(row.map(|row| InvoiceSync {
        option_index: row.option_index,
        kind: row.kind,
        invoice_ids: row
            .invoice_ids
            .split(',')
            .filter(|id| !id.is_empty())
            .map(String::from)
            .collect(),
        detail: serde_json::from_str(&row.detail_json).unwrap_or(Value::Null),
        source_hash: row.source_hash,
        enabled: row.enabled,
    })))
}

/// Link a quote's invoice to the quote
///
/// Saves the quote's invoice and how it is kept in step: after it is created
/// with "Keep in sync with the quote" ticked, after each sync (the new
/// `source_hash`), and with `enabled` off to stop. A quote has one; saving
/// replaces it.
#[crate::openapi::path(
    put,
    path = "/v1/quotes/{id}/invoice-sync",
    operation_id = "save_quote_invoice_sync",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    request_body = InvoiceSync,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Invalid kind, invoice ids, hash or detail", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_invoice_sync(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(quote_id): Path<i32>,
    JsonBody(input): JsonBody<InvoiceSync>,
) -> ApiResult<StatusCode> {
    if !matches!(input.kind.as_str(), "qbo" | "own") {
        return Err(ApiError::BadRequest("Kind must be qbo or own".into()));
    }
    if input.option_index < 0 {
        return Err(ApiError::BadRequest("Invalid option".into()));
    }
    let ids = &input.invoice_ids;
    let valid_id = |id: &String| {
        !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric())
    };
    if ids.is_empty()
        || ids.len() > MAX_INVOICES
        || !ids.iter().all(valid_id)
        || (input.kind == "own" && ids.len() != 1)
    {
        return Err(ApiError::BadRequest("Invalid invoice ids".into()));
    }
    let hash = input.source_hash.trim();
    if hash.is_empty() || hash.len() > 64 {
        return Err(ApiError::BadRequest("Invalid source hash".into()));
    }
    let detail = input.detail.to_string();
    if detail.len() > MAX_DETAIL {
        return Err(ApiError::BadRequest("Detail is too long".into()));
    }
    quote_exists(&state, user.company_id, quote_id).await?;
    sqlx::query(
        "INSERT INTO quote_invoice_sync
            (quote_id, company_id, option_index, kind, invoice_ids, detail_json,
             source_hash, enabled, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE
             option_index = VALUES(option_index), kind = VALUES(kind),
             invoice_ids = VALUES(invoice_ids), detail_json = VALUES(detail_json),
             source_hash = VALUES(source_hash), enabled = VALUES(enabled)",
    )
    .bind(quote_id)
    .bind(user.company_id)
    .bind(input.option_index)
    .bind(&input.kind)
    .bind(ids.join(","))
    .bind(detail)
    .bind(hash)
    .bind(input.enabled)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
