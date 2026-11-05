//! More of a customer's own page (see [`super::customer_view`]): who to
//! call, their documents, and where the crew is.
//!
//! - `GET /v1/public/customer-view/{company_id}/{view_id}/contact`   → their sales rep and the company
//! - `GET /v1/public/customer-view/{company_id}/{view_id}/documents` → contracts and quotes sent to them
//! - `GET /v1/public/customer-view/{company_id}/{view_id}/documents/{id}` → one contract as a PDF
//! - `GET /v1/public/customer-view/{company_id}/{view_id}/crew/{event_id}` → the crew on the way
//!
//! Every one needs a verified visitor (the `__customer_view` cookie).

use super::customer_view::{no_store, verified_customer};
use super::signatures::{NativeScope, native_document_of, signed_pdf_of};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::maps::Point;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use serde::Serialize;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(contact))
        .routes(routes!(documents))
        .routes(routes!(document))
        .routes(routes!(crew))
}

/// Contract statuses the customer sees: sent to them, opened, or signed.
const SHOWN_CONTRACTS: &str = "'sent', 'viewed', 'signed'";

/// The customer's sales rep.
#[derive(Debug, Serialize, ToSchema)]
pub struct ViewRep {
    /// Name.
    pub name: Option<String>,
    /// Work email.
    pub email: String,
    /// Their own phone, else their Granite Phone line, else their CloudTalk
    /// or `RingCentral` number (as the AI's emails sign).
    pub phone: Option<String>,
}

/// Who the customer deals with.
#[derive(Debug, Serialize, ToSchema)]
pub struct ViewContact {
    /// The company's name.
    pub company_name: Option<String>,
    /// The company's address.
    pub company_address: Option<String>,
    /// The customer's sales rep (their **Sales Rep**, else whoever sold
    /// their latest project); `None` when neither.
    pub rep: Option<ViewRep>,
}

#[derive(sqlx::FromRow)]
struct RepRow {
    id: i32,
    name: Option<String>,
    email: String,
    phone_number: Option<String>,
    cloudtalk_phone_number: Option<String>,
    ringcentral_phone_number: Option<String>,
}

/// Get who the customer deals with
///
/// Their sales rep's name, email and phone, and the company. Needs a
/// verified visitor.
#[crate::openapi::path(
    get,
    path = "/v1/public/customer-view/{company_id}/{view_id}/contact",
    operation_id = "get_customer_view_contact",
    tag = "customer-view",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("view_id" = String, Path, description = "The customer's page id (a UUID)"),
    ),
    responses(
        (status = 200, description = "The contact", body = ViewContact),
        (status = 401, description = "No code entered for this page", body = ErrorBody),
        (status = 404, description = "No such page", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn contact(
    State(state): State<AppState>,
    Path((company_id, view_id)): Path<(i32, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let customer = verified_customer(&state, &headers, company_id, &view_id).await?;
    let company: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT name, NULLIF(TRIM(address), '') FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(&state.pool)
            .await?;
    let rep: Option<RepRow> = sqlx::query_as(
        "SELECT u.id, NULLIF(TRIM(u.name), '') AS name, u.email, u.phone_number,
                u.cloudtalk_phone_number, u.ringcentral_phone_number
           FROM customers c
           JOIN users u ON u.id = COALESCE(c.sales_rep, (
                  SELECT s.seller_id FROM sales s
                   WHERE s.customer_id = c.id AND s.company_id = c.company_id
                   ORDER BY s.sale_date DESC, s.id DESC LIMIT 1))
          WHERE c.id = ? AND c.company_id = ? AND u.company_id = c.company_id
            AND u.is_deleted = 0",
    )
    .bind(customer.id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    let rep = match rep {
        Some(row) => {
            let line = api_crm::phone::texting::user_line(&state.pool, company_id, row.id)
                .await?
                .map(|line| line.e164);
            let phone = [
                row.phone_number,
                line,
                row.cloudtalk_phone_number,
                row.ringcentral_phone_number,
            ]
            .into_iter()
            .flatten()
            .map(|phone| phone.trim().to_string())
            .find(|phone| !phone.is_empty());
            Some(ViewRep {
                name: row.name,
                email: row.email,
                phone,
            })
        }
        None => None,
    };
    let (company_name, company_address) = company.map_or((None, None), |(n, a)| (Some(n), a));
    Ok(no_store(Json(ViewContact {
        company_name,
        company_address,
        rep,
    })))
}

/// A document sent to the customer.
#[derive(Debug, Serialize, ToSchema)]
pub struct ViewDocument {
    /// `contract` (sent for signature) or `quote`.
    pub kind: String,
    /// The signature request or the quote.
    pub id: i64,
    /// Its name.
    pub name: String,
    /// A contract's `sent`, `viewed` or `signed`; a quote's own status.
    pub status: String,
    /// When it was made (ISO 8601, UTC).
    pub created_at: Option<String>,
    /// A quote's page on this site (`/quote-portal/<token>`); a contract
    /// opens with `.../documents/{id}`.
    pub link: Option<String>,
}

#[derive(sqlx::FromRow)]
struct DocumentRow {
    id: i64,
    name: String,
    status: String,
    created_at: Option<String>,
    token: Option<String>,
}

/// List the customer's documents
///
/// The contracts sent to them for signature (sent, opened or signed; not
/// drafts, declined or expired ones) and the quotes emailed to them, newest
/// first. Needs a verified visitor.
#[crate::openapi::path(
    get,
    path = "/v1/public/customer-view/{company_id}/{view_id}/documents",
    operation_id = "list_customer_view_documents",
    tag = "customer-view",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("view_id" = String, Path, description = "The customer's page id (a UUID)"),
    ),
    responses(
        (status = 200, description = "The documents", body = [ViewDocument]),
        (status = 401, description = "No code entered for this page", body = ErrorBody),
        (status = 404, description = "No such page", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn documents(
    State(state): State<AppState>,
    Path((company_id, view_id)): Path<(i32, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let customer = verified_customer(&state, &headers, company_id, &view_id).await?;
    let contracts: Vec<DocumentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(id AS SIGNED) AS id, filename AS name, CAST(status AS CHAR) AS status,
                DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at, NULL AS token
           FROM signature_requests
          WHERE company_id = ? AND customer_id = ? AND CAST(status AS CHAR) IN ({SHOWN_CONTRACTS})
          ORDER BY created_at DESC, id DESC"
    )))
    .bind(company_id)
    .bind(customer.id)
    .fetch_all(&state.pool)
    .await?;
    let quotes: Vec<DocumentRow> = sqlx::query_as(
        "SELECT CAST(q.id AS SIGNED) AS id, COALESCE(NULLIF(TRIM(q.quote_name), ''), 'Quote') AS name,
                q.status, DATE_FORMAT(q.created_date, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                q.portal_token AS token
           FROM quotes q
          WHERE q.company_id = ? AND q.customer_id = ? AND q.deleted_at IS NULL
            AND q.portal_token IS NOT NULL
          ORDER BY q.created_date DESC, q.id DESC",
    )
    .bind(company_id)
    .bind(customer.id)
    .fetch_all(&state.pool)
    .await?;
    let mut list: Vec<ViewDocument> = contracts
        .into_iter()
        .map(|row| ViewDocument {
            kind: "contract".into(),
            id: row.id,
            name: row.name,
            status: row.status,
            created_at: row.created_at,
            link: None,
        })
        .chain(quotes.into_iter().map(|row| ViewDocument {
            kind: "quote".into(),
            id: row.id,
            name: row.name,
            status: row.status,
            created_at: row.created_at,
            link: row.token.map(|token| format!("/quote-portal/{token}")),
        }))
        .collect();
    // ISO dates sort as text; newest first.
    list.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(no_store(Json(list)))
}

/// Open one of the customer's contracts
///
/// The PDF of a contract sent to this customer: signed once they signed it,
/// else as sent. Opened in a browser tab: errors are plain text. Needs a
/// verified visitor.
#[crate::openapi::path(
    get,
    path = "/v1/public/customer-view/{company_id}/{view_id}/documents/{id}",
    operation_id = "open_customer_view_document",
    tag = "customer-view",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("view_id" = String, Path, description = "The customer's page id (a UUID)"),
        ("id" = i64, Path, description = "Signature request id"),
    ),
    responses(
        (status = 200, description = "The PDF", content_type = "application/pdf"),
        (status = 302, description = "Over 4 MB: redirect to a presigned link"),
        (status = 401, description = "No code entered for this page", body = ErrorBody),
        (status = 404, description = "No such page, or not this customer's contract", body = ErrorBody),
        (status = 502, description = "The document could not be downloaded", content_type = "text/plain"),
        (status = 503, description = "The file cannot be read right now", content_type = "text/plain"),
    )
)]
pub async fn document(
    State(state): State<AppState>,
    Path((company_id, view_id, id)): Path<(i32, String, i64)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let customer = verified_customer(&state, &headers, company_id, &view_id).await?;
    let provider: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT provider FROM signature_requests
          WHERE id = ? AND company_id = ? AND customer_id = ?
            AND CAST(status AS CHAR) IN ({SHOWN_CONTRACTS})"
    )))
    .bind(id)
    .bind(company_id)
    .bind(customer.id)
    .fetch_optional(&state.pool)
    .await?;
    let customer_id = i64::from(customer.id);
    match provider.as_deref() {
        None => Err(ApiError::NotFound),
        Some("native") => {
            native_document_of(&state, company_id, id, NativeScope::Customer(customer_id)).await
        }
        Some(_) => signed_pdf_of(&state, company_id, id, Some(customer_id)).await,
    }
}

/// The crew on the way to one of the customer's visits.
#[derive(Debug, Serialize, ToSchema)]
pub struct ViewCrew {
    /// Who is coming.
    pub crew_name: Option<String>,
    /// Where their phone was last.
    pub latitude: f64,
    /// Where their phone was last.
    pub longitude: f64,
    /// When (ISO 8601, UTC).
    pub recorded_at: String,
    /// Driving minutes from there to the visit in today's traffic; `None`
    /// when Google cannot tell.
    pub minutes_away: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct CrewRow {
    crew_name: Option<String>,
    location: Option<String>,
    latitude: f64,
    longitude: f64,
    recorded_at: String,
}

/// Most minutes since the crew's phone last reported for the map to show.
const CREW_FRESH_MINUTES: i64 = 30;

/// Where the crew is
///
/// Only while the crew of this customer's visit is on the way (they tapped
/// **On my way** and have not arrived) and their phone reported in the last
/// 30 minutes; otherwise 404. Needs a verified visitor.
#[crate::openapi::path(
    get,
    path = "/v1/public/customer-view/{company_id}/{view_id}/crew/{event_id}",
    operation_id = "get_customer_view_crew",
    tag = "customer-view",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("view_id" = String, Path, description = "The customer's page id (a UUID)"),
        ("event_id" = i32, Path, description = "The visit (event id)"),
    ),
    responses(
        (status = 200, description = "The crew", body = ViewCrew),
        (status = 401, description = "No code entered for this page", body = ErrorBody),
        (status = 404, description = "No such page, or the crew is not on the way", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn crew(
    State(state): State<AppState>,
    Path((company_id, view_id, event_id)): Path<(i32, String, i32)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let customer = verified_customer(&state, &headers, company_id, &view_id).await?;
    let row: Option<CrewRow> = sqlx::query_as(
        "SELECT NULLIF(TRIM(a.name), '') AS crew_name, NULLIF(TRIM(e.location), '') AS location,
                cp.latitude, cp.longitude,
                DATE_FORMAT(cp.recorded_at, '%Y-%m-%dT%H:%i:%sZ') AS recorded_at
           FROM events e
           JOIN users u ON u.id = e.created_user_id
           JOIN users a ON a.id = e.assigned_user_id AND a.company_id = u.company_id
           JOIN crew_positions cp ON cp.user_id = a.id AND cp.company_id = u.company_id
          WHERE e.id = ? AND e.customer_id = ? AND u.company_id = ?
            AND e.deleted_date IS NULL AND e.crew_status = 'on_my_way'
            AND cp.recorded_at >= UTC_TIMESTAMP() - INTERVAL ? MINUTE",
    )
    .bind(event_id)
    .bind(customer.id)
    .bind(company_id)
    .bind(CREW_FRESH_MINUTES)
    .fetch_optional(&state.pool)
    .await?;
    let row = row.ok_or(ApiError::NotFound)?;
    let here = Point {
        lat: row.latitude,
        lng: row.longitude,
    };
    let minutes_away = match row.location.as_deref() {
        Some(address) => state.maps.driving_minutes(here, address).await,
        None => None,
    };
    Ok(no_store(Json(ViewCrew {
        crew_name: row.crew_name,
        latitude: row.latitude,
        longitude: row.longitude,
        recorded_at: row.recorded_at,
        minutes_away,
    })))
}
