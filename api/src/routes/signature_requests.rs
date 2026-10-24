//! Documents sent for e-signature, across the company (the eSignatures pages).
//!
//! - `GET  /v1/signature-requests?scope=` → the caller's (or, for admins, the company's)
//! - `POST /v1/signature-requests/{id}/cancel` → stop a request the caller sent
//!
//! Replaces the Remix `employee.signed-documents` and `admin.signed-documents`
//! loaders and the cancel action (`cancelSignatureRequest`).

use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::entitlements::AddonKey;
use crate::error::{AddonRequiredBody, ApiError, ApiResult, ErrorBody};
use crate::routes::customers::signatures::{RefreshScope, refresh};
use crate::signwell::SignWell;
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use lambda_http::tracing;
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
/// With `notify_customer`, the signer is emailed that it was cancelled (from
/// the sender, in the invitation's thread when there is one) and the answer
/// says whether that email went out.
#[utoipa::path(
    post,
    path = "/v1/signature-requests/{id}/cancel",
    operation_id = "cancel_signature_request",
    tag = "customers",
    params(("id" = i64, Path, description = "Signature request id")),
    request_body(content = CancelInput, description = "Optional; no body: the customer is not told"),
    responses(
        (status = 204, description = "Cancelled"),
        (status = 200, description = "Cancelled; whether the customer was emailed", body = Cancelled),
        (status = 400, description = "Already finished, or SignWell refused (its message)", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 404, description = "No such request sent by the caller", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
#[allow(clippy::too_many_lines)]
pub async fn cancel_request(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    let input: CancelInput = if body.iter().all(u8::is_ascii_whitespace) {
        CancelInput::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| ApiError::BadRequest("Send { \"notify_customer\": true|false }.".into()))?
    };
    let row: Option<(String, String, Option<String>, String, String, String)> = sqlx::query_as(
        "SELECT CAST(status AS CHAR), provider, provider_document_id, filename, signer_name, signer_email
           FROM signature_requests
          WHERE id = ? AND company_id = ? AND created_by_user_id = ? LIMIT 1",
    )
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await?;
    let (status, provider, document_id, filename, signer_name, signer_email) =
        row.ok_or(ApiError::NotFound)?;
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
    if !input.notify_customer {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let email = cancel_email(
        &state,
        &user,
        id,
        &provider,
        &filename,
        &signer_name,
        &signer_email,
    )
    .await;
    let customer_notified = match state.webhooks.send_email(&email).await {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(
                request_id = id,
                ?error,
                "signature cancellation email not sent"
            );
            false
        }
    };
    Ok(Json(Cancelled { customer_notified }).into_response())
}

/// Body of `POST /v1/signature-requests/{id}/cancel`.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelInput {
    /// Email the signer that the request was cancelled (default false).
    #[serde(default)]
    pub notify_customer: bool,
}

/// A cancelled request, when the customer was to be told.
#[derive(Debug, Serialize, ToSchema)]
pub struct Cancelled {
    /// False when the email to the customer could not be sent.
    pub customer_notified: bool,
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `Name <address>` or a bare address.
fn sender_of(from: &str) -> (Option<String>, String) {
    match (from.rfind('<'), from.rfind('>')) {
        (Some(open), Some(close)) if open < close => {
            let name = from[..open].trim().trim_matches('"').trim();
            (
                Some(name.to_string()).filter(|n| !n.is_empty()),
                from[open + 1..close].trim().to_string(),
            )
        }
        _ => (None, from.trim().to_string()),
    }
}

/// The signer's notice: from whoever sent the invitation, replying in its
/// thread (native); from the caller otherwise.
async fn cancel_email(
    state: &AppState,
    user: &SessionUser,
    request_id: i64,
    provider: &str,
    filename: &str,
    signer_name: &str,
    signer_email: &str,
) -> OutgoingEmail {
    let invitation = match (&state.native_signing, provider) {
        (Some(signing), "native") => signing
            .lifecycle_mail(&state.pool, request_id, i64::from(user.company_id))
            .await
            .ok(),
        _ => None,
    };
    let (from_name, from_address) = invitation.as_ref().map_or_else(
        || (user.name.clone(), user.email.clone()),
        |mail| sender_of(&mail.snapshot.from),
    );
    let greeting = signer_name.split_whitespace().next().unwrap_or_default();
    let sender = from_name.clone().unwrap_or_else(|| from_address.clone());
    OutgoingEmail {
        from_name,
        from_address,
        to: vec![signer_email.to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: format!("Signature request canceled: {filename}"),
        body: format!(
            "<p>Hi{},</p><p>The signature request for <b>{}</b> has been canceled, so you no longer need to sign it and its link no longer works.</p><p>{}</p>",
            if greeting.is_empty() {
                String::new()
            } else {
                format!(" {}", escape_html(greeting))
            },
            escape_html(filename),
            escape_html(&sender),
        ),
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: invitation
            .as_ref()
            .and_then(|mail| mail.snapshot.in_reply_to.clone()),
        references: invitation
            .map(|mail| mail.snapshot.references)
            .unwrap_or_default(),
        attachments: Vec::new(),
        transactional: true,
    }
}
