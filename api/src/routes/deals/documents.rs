//! What happened to a deal's quotes and contracts, for its history.
//!
//! Port of the CRM's `dealDocumentHistory.server.ts`.

use super::queries::deal_in_company;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use sqlx::{FromRow, MySqlPool};
use utoipa::ToSchema;

/// What happened to the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DocumentEventKind {
    /// A quote was emailed to the customer.
    QuoteEmailed,
    /// A quote was printed (or saved as a PDF).
    QuotePrinted,
    /// A contract was sent for signing.
    ContractSent,
    /// The customer signed a contract.
    ContractSigned,
}

/// One quote or contract event of a deal.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DocumentEvent {
    /// Stable key, e.g. `quote-email-12` or `contract-signed-7`.
    pub id: String,
    /// What happened.
    pub kind: DocumentEventKind,
    /// The quote's name (`Quote` when it has none), or the contract's file
    /// name without `.pdf`.
    pub title: String,
    /// Recipients of an emailed quote, the form a quote was printed with, or
    /// the contract's signer. Null when unknown.
    pub detail: Option<String>,
    /// When it happened, UTC (`2026-05-01T14:30:00Z`).
    pub at: String,
    /// Who sent or printed it. Null for a signing, or when unknown.
    pub by: Option<String>,
    /// The quote to open, for quote events; null for contracts.
    pub quote_id: Option<i64>,
}

#[derive(FromRow)]
struct QuoteRow {
    id: i64,
    quote_name: Option<String>,
}

#[derive(FromRow)]
struct QuoteEventRow {
    id: i64,
    quote_id: i64,
    detail: Option<String>,
    at: String,
    actor: Option<String>,
}

#[derive(FromRow)]
struct ContractRow {
    id: i64,
    deal_id: Option<i64>,
    filename: String,
    signer_name: String,
    status: String,
    sent_at: String,
    changed_at: String,
    actor: Option<String>,
}

const ISO: &str = "%Y-%m-%dT%H:%i:%sZ";

fn quote_name(quotes: &[QuoteRow], id: i64) -> String {
    quotes
        .iter()
        .find(|quote| quote.id == id)
        .and_then(|quote| quote.quote_name.as_deref())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("Quote")
        .to_string()
}

/// `name` without a trailing `.pdf` (any case).
fn without_pdf(name: &str) -> &str {
    let cut = name.len().saturating_sub(4);
    match name.get(cut..) {
        Some(ext) if ext.eq_ignore_ascii_case(".pdf") => &name[..cut],
        _ => name,
    }
}

/// The CRM's `cleanFileName`: characters a file name can't hold become spaces.
fn clean_file_name(name: &str) -> String {
    name.chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Whether `filename` is the contract of one of `quotes`: the quote page names
/// contracts `<contract form> - <quote name>.pdf`.
fn is_quote_contract(filename: &str, contract_forms: &[String], quotes: &[QuoteRow]) -> bool {
    let base = without_pdf(filename).trim().to_lowercase();
    let forms: Vec<String> = std::iter::once("Contract")
        .chain(contract_forms.iter().map(String::as_str))
        .map(|form| clean_file_name(form).to_lowercase())
        .collect();
    quotes.iter().any(|quote| {
        let name = clean_file_name(quote.quote_name.as_deref().unwrap_or("Quote")).to_lowercase();
        forms
            .iter()
            .any(|form| base == clean_file_name(&format!("{form} - {name}")))
    })
}

async fn quote_events(
    pool: &MySqlPool,
    quotes: &[QuoteRow],
    sql: &'static str,
    company_id: i32,
    deal_id: i64,
    kind: DocumentEventKind,
    key: &str,
) -> Result<Vec<DocumentEvent>, sqlx::Error> {
    let rows: Vec<QuoteEventRow> = sqlx::query_as(sql)
        .bind(ISO)
        .bind(company_id)
        .bind(deal_id)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| DocumentEvent {
            id: format!("{key}-{}", row.id),
            kind,
            title: quote_name(quotes, row.quote_id),
            detail: row.detail,
            at: row.at,
            by: row.actor,
            quote_id: Some(row.quote_id),
        })
        .collect())
}

const QUOTE_EMAILS_SQL: &str = "SELECT CAST(e.id AS SIGNED) AS id, CAST(e.quote_id AS SIGNED) AS quote_id,
            e.recipients AS detail, DATE_FORMAT(e.sent_at, ?) AS at, u.name AS actor
       FROM quote_emails e
       JOIN quotes q ON q.id = e.quote_id
       LEFT JOIN users u ON u.id = e.sent_by
      WHERE q.company_id = ? AND q.deal_id = ? AND q.deleted_at IS NULL";

const QUOTE_PRINTS_SQL: &str = "SELECT CAST(p.id AS SIGNED) AS id, CAST(p.quote_id AS SIGNED) AS quote_id,
            p.form AS detail, DATE_FORMAT(p.printed_at, ?) AS at, u.name AS actor
       FROM quote_prints p
       JOIN quotes q ON q.id = p.quote_id
       LEFT JOIN users u ON u.id = p.printed_by
      WHERE q.company_id = ? AND q.deal_id = ? AND q.deleted_at IS NULL";

/// Contracts tied to the deal, or sent from one of its quotes (by file name).
async fn contract_events(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i64,
    deal_id: i64,
    quotes: &[QuoteRow],
) -> Result<Vec<DocumentEvent>, sqlx::Error> {
    let rows: Vec<ContractRow> = sqlx::query_as(
        "SELECT CAST(sr.id AS SIGNED) AS id,
                CAST(COALESCE(sr.deal_id, e.deal_id) AS SIGNED) AS deal_id,
                sr.filename, sr.signer_name, CAST(sr.status AS CHAR) AS status,
                DATE_FORMAT(sr.created_at, ?) AS sent_at,
                DATE_FORMAT(sr.updated_at, ?) AS changed_at, u.name AS actor
           FROM signature_requests sr
           LEFT JOIN emails e ON e.id = sr.email_id
           LEFT JOIN users u ON u.id = sr.created_by_user_id
          WHERE sr.company_id = ? AND sr.customer_id = ? AND sr.status <> 'draft'",
    )
    .bind(ISO)
    .bind(ISO)
    .bind(company_id)
    .bind(customer_id)
    .fetch_all(pool)
    .await?;
    // Forms made before contracts existed have no kind: they are quotes.
    let contract_forms: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM quote_forms
          WHERE company_id = ? AND deleted_at IS NULL
            AND JSON_UNQUOTE(JSON_EXTRACT(config_json, '$.kind')) = 'contract'",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;

    let mut events = Vec::new();
    for row in rows {
        let mine = row.deal_id == Some(deal_id)
            || (row.deal_id.is_none() && is_quote_contract(&row.filename, &contract_forms, quotes));
        if !mine {
            continue;
        }
        let title = without_pdf(&row.filename).to_string();
        events.push(DocumentEvent {
            id: format!("contract-sent-{}", row.id),
            kind: DocumentEventKind::ContractSent,
            title: title.clone(),
            detail: Some(row.signer_name.clone()),
            at: row.sent_at,
            by: row.actor,
            quote_id: None,
        });
        // Nothing updates a signed request after signing, so its last change
        // is the signing.
        if row.status == "signed" {
            events.push(DocumentEvent {
                id: format!("contract-signed-{}", row.id),
                kind: DocumentEventKind::ContractSigned,
                title,
                detail: Some(row.signer_name),
                at: row.changed_at,
                by: None,
                quote_id: None,
            });
        }
    }
    Ok(events)
}

/// MySQL's "table doesn't exist".
fn is_missing_table(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| code == "42S02")
}

/// List a deal's quote and contract history
///
/// Quotes of the deal that were emailed or printed, and contracts sent for
/// signing (tied to the deal, or named after one of its quotes:
/// `<contract form> - <quote name>.pdf`) and signed. Views are left out.
/// Unsorted: the deal page merges them into its history by `at`.
#[utoipa::path(
    get,
    path = "/v1/deals/{id}/document-events",
    operation_id = "list_deal_document_events",
    tag = "deals",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 200, description = "Quote and contract events", body = Vec<DocumentEvent>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<DocumentEvent>>> {
    if !deal_in_company(&state.pool, user.company_id, id).await? {
        return Err(ApiError::NotFound);
    }
    let customer_id: i64 =
        sqlx::query_scalar("SELECT CAST(customer_id AS SIGNED) FROM deals WHERE id = ?")
            .bind(id)
            .fetch_one(&state.pool)
            .await?;
    let quotes: Vec<QuoteRow> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, quote_name FROM quotes
          WHERE company_id = ? AND deal_id = ? AND deleted_at IS NULL",
    )
    .bind(user.company_id)
    .bind(id)
    .fetch_all(&state.pool)
    .await?;

    let mut events = quote_events(
        &state.pool,
        &quotes,
        QUOTE_EMAILS_SQL,
        user.company_id,
        id,
        DocumentEventKind::QuoteEmailed,
        "quote-email",
    )
    .await?;
    // quote_prints arrives with its migration; until then there is nothing to show.
    match quote_events(
        &state.pool,
        &quotes,
        QUOTE_PRINTS_SQL,
        user.company_id,
        id,
        DocumentEventKind::QuotePrinted,
        "quote-print",
    )
    .await
    {
        Ok(printed) => events.extend(printed),
        Err(error) if is_missing_table(&error) => {}
        Err(error) => return Err(error.into()),
    }
    events.extend(contract_events(&state.pool, user.company_id, customer_id, id, &quotes).await?);
    Ok(Json(events))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(id: i64, name: Option<&str>) -> QuoteRow {
        QuoteRow {
            id,
            quote_name: name.map(str::to_string),
        }
    }

    #[test]
    fn contract_files_match_their_quote() {
        let quotes = [quote(1, Some("Smith: Kitchen")), quote(2, None)];
        let forms = ["Install Agreement".to_string()];
        assert!(is_quote_contract("Contract - Smith  Kitchen.pdf", &forms, &quotes));
        assert!(is_quote_contract("install agreement - smith  kitchen.PDF", &forms, &quotes));
        assert!(is_quote_contract("Contract - Quote.pdf", &forms, &quotes));
        assert!(!is_quote_contract("Template approval.pdf", &forms, &quotes));
        assert_eq!(without_pdf("Contract - A.PdF"), "Contract - A");
        assert_eq!(without_pdf("pdf"), "pdf");
        assert_eq!(quote_name(&quotes, 2), "Quote");
        assert_eq!(quote_name(&quotes, 1), "Smith: Kitchen");
    }
}
