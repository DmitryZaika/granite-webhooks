//! Documents sent for e-signature, across the company (the eSignatures pages).
//!
//! - `GET  /v1/signature-requests?scope=` → the caller's (or, for admins, the company's)
//! - `POST /v1/signature-requests/{id}/cancel` → stop a request the caller sent
//!
//! Replaces the Remix `employee.signed-documents` and `admin.signed-documents`
//! loaders and the cancel action (`cancelSignatureRequest`).

use crate::auth::EmployeeUser;
use crate::entitlements::AddonKey;
use crate::error::{AddonRequiredBody, ApiError, ApiResult, ErrorBody};
use crate::routes::customers::signatures::{RefreshScope, refresh};
use crate::signwell::SignWell;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_requests))
        .routes(routes!(cancel_request))
}

/// `?scope=` of `GET /v1/signature-requests`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListScope {
    /// `mine` (default): what the caller sent; `company`: everyone's (admins).
    #[serde(default)]
    pub scope: Option<String>,
}

/// A document sent for signature.
#[derive(Debug, Serialize, ToSchema)]
pub struct SentRequest {
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
    /// `signwell` or `native`.
    pub provider: String,
    /// The customer's name, or null.
    pub customer_name: Option<String>,
    /// Who sent it, or null.
    pub sender_name: Option<String>,
    /// When it was sent.
    pub created_at: Option<DateTime<Utc>>,
    /// Last change (when signed, the signing time).
    pub updated_at: Option<DateTime<Utc>>,
    /// Where to open it: an API path (`/v1/...`) to open on the API's host,
    /// else the original file's URL.
    pub signed_url: String,
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
    customer_name: Option<String>,
    sender_name: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

/// List documents sent for signature
///
/// Newest first, with the customer and sender names. `mine` (default):
/// what the caller sent; `company` (admins): the whole company. Open
/// `SignWell` requests are checked with `SignWell` first. Needs the
/// Signatures add-on; cancelling a request stays open without it.
#[utoipa::path(
    get,
    path = "/v1/signature-requests",
    operation_id = "list_signature_requests",
    tag = "customers",
    params(ListScope),
    responses(
        (status = 200, description = "The requests", body = [SentRequest]),
        (status = 400, description = "Unknown scope", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, not an admin for `company`, or the plan lacks the Signatures add-on (`addon`: `signatures`)", body = AddonRequiredBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_requests(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<ListScope>,
) -> ApiResult<Json<Vec<SentRequest>>> {
    user.plan.require(AddonKey::Signatures)?;
    let sent_by = match query.scope.as_deref() {
        None | Some("mine") => Some(user.id),
        Some("company") if user.is_admin || user.is_superuser => None,
        Some("company") => return Err(ApiError::Forbidden),
        Some(_) => return Err(ApiError::BadRequest("scope must be mine or company".into())),
    };
    refresh(
        &state,
        user.company_id,
        sent_by.map_or(RefreshScope::Company, RefreshScope::SentBy),
    )
    .await?;
    let mut sql = sqlx::QueryBuilder::new(
        "SELECT CAST(sr.id AS SIGNED) AS id, sr.filename, sr.file_url, sr.signer_name, sr.signer_email,
                CAST(sr.status AS CHAR) AS status, sr.provider, sr.provider_document_id,
                c.name AS customer_name, u.name AS sender_name, sr.created_at, sr.updated_at
           FROM signature_requests sr
           LEFT JOIN customers c ON c.id = sr.customer_id AND c.company_id = sr.company_id
           LEFT JOIN users u ON u.id = sr.created_by_user_id AND u.company_id = sr.company_id
          WHERE sr.company_id = ",
    );
    sql.push_bind(user.company_id);
    if let Some(id) = sent_by {
        sql.push(" AND sr.created_by_user_id = ").push_bind(id);
    }
    sql.push(" ORDER BY sr.created_at DESC, sr.id DESC");
    let rows: Vec<Row> = sql.build_query_as().fetch_all(&state.pool).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let signed_url = if row.provider == "native" {
            format!("/v1/signatures/{}/document", row.id)
        } else if row.status == "signed"
            && row
                .provider_document_id
                .as_deref()
                .is_some_and(|id| !id.trim().is_empty())
        {
            format!("/v1/signature-requests/{}/signed-pdf", row.id)
        } else {
            state
                .storage
                .readable_url(&row.file_url, Some("application/pdf"))
                .await
        };
        out.push(SentRequest {
            id: row.id,
            filename: row.filename,
            signer_name: row.signer_name,
            signer_email: row.signer_email,
            status: row.status,
            provider: row.provider,
            customer_name: row.customer_name,
            sender_name: row.sender_name,
            created_at: row.created_at,
            updated_at: row.updated_at,
            signed_url,
        });
    }
    Ok(Json(out))
}

/// Cancel a signature request
///
/// Stops a request the caller sent that is not finished: a `SignWell`
/// document is deleted at `SignWell` (its link stops working); a native one
/// is revoked here and gets no more reminders. The request becomes `expired`.
#[utoipa::path(
    post,
    path = "/v1/signature-requests/{id}/cancel",
    operation_id = "cancel_signature_request",
    tag = "customers",
    params(("id" = i64, Path, description = "Signature request id")),
    responses(
        (status = 204, description = "Cancelled"),
        (status = 400, description = "Already finished, or SignWell refused (its message)", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 404, description = "No such request sent by the caller", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn cancel_request(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT CAST(status AS CHAR), provider, provider_document_id FROM signature_requests
          WHERE id = ? AND company_id = ? AND created_by_user_id = ? LIMIT 1",
    )
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await?;
    let (status, provider, document_id) = row.ok_or(ApiError::NotFound)?;
    if matches!(status.as_str(), "signed" | "declined" | "expired") {
        return Err(ApiError::BadRequest(
            "This request is already finished.".into(),
        ));
    }
    let document_id = document_id.unwrap_or_default();
    if provider == "signwell" && !document_id.trim().is_empty() {
        let key: Option<Option<String>> =
            sqlx::query_scalar("SELECT signwell_api_key FROM company WHERE id = ?")
                .bind(user.company_id)
                .fetch_optional(&state.pool)
                .await?;
        let Some(key) = key.flatten().filter(|k| !k.trim().is_empty()) else {
            return Err(ApiError::BadRequest(
                "This request was sent through SignWell, so it has to be cancelled there. Add the SignWell API key in Superuser › Companies › Edit company, then cancel again.".into(),
            ));
        };
        SignWell::new(&state.signwell_base)
            .delete(key.trim(), &document_id)
            .await
            .map_err(ApiError::BadRequest)?;
    }
    let mut tx = state.pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE signature_requests SET status = 'expired'
          WHERE id = ? AND company_id = ? AND created_by_user_id = ? AND status IN ('draft', 'sent', 'viewed')",
    )
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(ApiError::BadRequest(
            "This request is already finished.".into(),
        ));
    }
    if provider == "native" {
        sqlx::query(
            "UPDATE native_signature_documents SET revoked_at = CURRENT_TIMESTAMP(3)
              WHERE request_id = ? AND company_id = ? AND revoked_at IS NULL AND signed_at IS NULL",
        )
        .bind(id)
        .bind(user.company_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE native_signature_jobs SET state = 'cancelled', lease_owner = NULL, lease_until = NULL
              WHERE request_id = ? AND company_id = ? AND state IN ('pending', 'retry', 'blocked')",
        )
        .bind(id)
        .bind(user.company_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
