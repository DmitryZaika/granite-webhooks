//! `GET /v1/customers/{id}/signature-requests`: documents sent to a customer
//! for e-signature, with their status freshened from `SignWell`.
//! Remix: the loader of `employee.customers.info.$customerId.documents`.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

/// Most open requests checked with `SignWell` per call.
const MAX_REFRESH: i64 = 50;

/// A document sent for signature.
#[derive(Debug, Serialize, ToSchema)]
pub struct SignatureRequest {
    /// The request.
    pub id: i64,
    /// Document name.
    pub filename: String,
    /// Who signs.
    pub signer_name: String,
    /// Their email.
    pub signer_email: String,
    /// `draft`, `sent`, `viewed`, `signed`, `declined` or `expired`.
    pub status: String,
    /// When it was sent.
    pub created_at: Option<DateTime<Utc>>,
    /// Where to open it: the signed PDF or native document through the web
    /// app, else the original file.
    pub view_url: String,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    filename: String,
    file_url: String,
    signer_name: String,
    signer_email: String,
    status: String,
    provider: String,
    provider_document_id: Option<String>,
    created_at: Option<DateTime<Utc>>,
}

const RANKS: [(&str, u8); 6] = [
    ("draft", 0),
    ("sent", 1),
    ("viewed", 2),
    ("signed", 3),
    ("declined", 3),
    ("expired", 3),
];

fn rank(status: &str) -> u8 {
    RANKS
        .iter()
        .find(|(name, _)| *name == status)
        .map_or(1, |(_, rank)| *rank)
}

fn terminal(status: &str) -> bool {
    matches!(status, "signed" | "declined" | "expired")
}

/// A status may only move forward, and never out of a final one.
pub fn can_advance(current: &str, next: &str) -> bool {
    current != next && !terminal(current) && rank(next) >= rank(current)
}

fn document_status(status: &str) -> Option<&'static str> {
    match status.trim().to_lowercase().as_str() {
        "completed" | "manually completed" => Some("signed"),
        "viewed" => Some("viewed"),
        "declined" => Some("declined"),
        "canceled" | "cancelled" | "expired" => Some("expired"),
        "draft" => Some("draft"),
        "sent" | "sending" | "pending" | "created" => Some("sent"),
        _ => None,
    }
}

/// The request status a `SignWell` document reports.
pub fn status_from_document(document: &Value) -> Option<&'static str> {
    let from_document = document_status(document["status"].as_str().unwrap_or(""));
    if matches!(from_document, Some("signed" | "declined" | "expired")) {
        return from_document;
    }
    let recipient = document["recipients"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|recipient| recipient["status"].as_str())
        .map(|status| status.trim().to_lowercase())
        .unwrap_or_default();
    match recipient.as_str() {
        "completed" | "signed" => Some("signed"),
        "declined" => Some("declined"),
        "viewed" | "in progress" => Some("viewed"),
        _ => from_document,
    }
}

async fn refresh(state: &AppState, company_id: i32, customer_id: i32) -> Result<(), sqlx::Error> {
    let open: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), CAST(status AS CHAR), provider_document_id FROM signature_requests
          WHERE company_id = ? AND customer_id = ? AND provider = 'signwell'
            AND status IN ('draft', 'sent', 'viewed') AND TRIM(COALESCE(provider_document_id, '')) <> ''
          ORDER BY id DESC LIMIT ?",
    )
    .bind(company_id)
    .bind(customer_id)
    .bind(MAX_REFRESH)
    .fetch_all(&state.pool)
    .await?;
    if open.is_empty() {
        return Ok(());
    }
    let company_key: Option<Option<String>> =
        sqlx::query_scalar("SELECT signwell_api_key FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some(key) = company_key
        .flatten()
        .or_else(|| std::env::var("SIGNWELL_API_KEY").ok())
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
    else {
        return Ok(());
    };
    for (id, current, document_id) in open {
        let encoded = percent_encoding::utf8_percent_encode(
            document_id.trim(),
            percent_encoding::NON_ALPHANUMERIC,
        );
        let document: Option<Value> = async {
            state
                .telephony_http
                .get(format!("{}/documents/{encoded}", state.signwell_base))
                .header("X-Api-Key", &key)
                .header("Accept", "application/json")
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?
                .json()
                .await
                .ok()
        }
        .await;
        let Some(next) = document.as_ref().and_then(status_from_document) else {
            continue;
        };
        if can_advance(&current, next) {
            sqlx::query("UPDATE signature_requests SET status = ? WHERE id = ?")
                .bind(next)
                .bind(id)
                .execute(&state.pool)
                .await?;
        }
    }
    Ok(())
}

/// List a customer's signature requests
///
/// Documents sent to the customer for e-signature, newest first. Open
/// `SignWell` requests are checked with `SignWell` first.
#[utoipa::path(
    get,
    path = "/v1/customers/{id}/signature-requests",
    operation_id = "list_customer_signature_requests",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id")),
    responses(
        (status = 200, description = "Requests", body = [SignatureRequest]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn signature_requests(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(customer_id): Path<i32>,
) -> ApiResult<Json<Vec<SignatureRequest>>> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    found.ok_or(ApiError::NotFound)?;
    refresh(&state, user.company_id, customer_id).await?;
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, filename, file_url, signer_name, signer_email,
                CAST(status AS CHAR) AS status, provider, provider_document_id, created_at
           FROM signature_requests WHERE company_id = ? AND customer_id = ?
          ORDER BY created_at DESC, id DESC",
    )
    .bind(user.company_id)
    .bind(customer_id)
    .fetch_all(&state.pool)
    .await?;
    let mut requests = Vec::with_capacity(rows.len());
    for row in rows {
        let view_url = if row.provider == "native" {
            format!(
                "/api/signatures/{}/document?customerId={customer_id}",
                row.id
            )
        } else if row.status == "signed"
            && row
                .provider_document_id
                .as_deref()
                .is_some_and(|id| !id.is_empty())
        {
            format!("/api/signature-requests/{}/signed-pdf", row.id)
        } else {
            state
                .storage
                .readable_url(&row.file_url, Some("application/pdf"))
                .await
        };
        requests.push(SignatureRequest {
            id: row.id,
            filename: row.filename,
            signer_name: row.signer_name,
            signer_email: row.signer_email,
            status: row.status,
            created_at: row.created_at,
            view_url,
        });
    }
    Ok(Json(requests))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn signwell_statuses_map_like_the_crm() {
        assert_eq!(
            status_from_document(&json!({ "status": "Completed" })),
            Some("signed")
        );
        assert_eq!(
            status_from_document(
                &json!({ "status": "Sent", "recipients": [{ "status": "viewed" }] })
            ),
            Some("viewed")
        );
        assert_eq!(
            status_from_document(&json!({ "status": "pending" })),
            Some("sent")
        );
        assert!(can_advance("sent", "viewed"));
        assert!(!can_advance("viewed", "sent"));
        assert!(!can_advance("signed", "declined"));
    }
}
