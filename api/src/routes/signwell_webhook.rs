//! `SignWell`'s webhook: a document sent for signature was viewed, signed,
//! declined or cancelled.
//!
//! - `GET /v1/webhooks/signwell` → `{ ok: true }` (`SignWell` checks the URL)
//! - `POST /v1/webhooks/signwell` → the request's status moves forward; the
//!   sender is emailed when it was viewed, signed or declined, and a signed
//!   contract sells its quote (the quote is sold, its deal won with the
//!   option's total).
//!
//! The call is trusted with `SIGNWELL_WEBHOOK_SECRET` (as `?token=` or an
//! `x-webhook-token` header) or with `SignWell`'s event hash, an HMAC-SHA256
//! of `<type>@<time>` keyed with `SIGNWELL_WEBHOOK_ID`.
//!
//! Replaces the Remix `api.signwell.webhook` route
//! (`applySignWellWebhookEvent`, `notifySignatureEvent`,
//! `sellQuoteForSignedContract`).

use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::quote_pricing::{self, js::num_str};
use crate::routes::emails::signature_mail::{Branding, status_html};
use crate::state::AppState;
use crate::webhooks_client::{OutgoingAttachment, OutgoingEmail};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use hmac::{Hmac, KeyInit, Mac};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use sqlx::MySqlPool;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(check, receive))
}

/// The webhook was taken.
#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookOk {
    /// Always true.
    pub ok: bool,
}

/// How the call proves it comes from `SignWell`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct WebhookQuery {
    /// `SIGNWELL_WEBHOOK_SECRET`, when the webhook URL carries it.
    pub token: Option<String>,
}

/// Check the webhook URL
///
/// Answers `SignWell` checking that the webhook URL is there.
#[utoipa::path(
    get,
    path = "/v1/webhooks/signwell",
    operation_id = "check_signwell_webhook",
    tag = "signatures",
    responses((status = 200, description = "The URL is there", body = WebhookOk))
)]
pub async fn check() -> Json<WebhookOk> {
    Json(WebhookOk { ok: true })
}

/// Receive a `SignWell` event
///
/// A document's event (`document_viewed`, `document_completed`,
/// `document_declined`, `document_canceled`…): its signature request moves
/// forward, the sender is notified, and a signed contract sells its quote.
/// Needs `?token=` / `x-webhook-token` (`SIGNWELL_WEBHOOK_SECRET`) or the
/// event's hash (`SIGNWELL_WEBHOOK_ID`); no session.
#[utoipa::path(
    post,
    path = "/v1/webhooks/signwell",
    operation_id = "receive_signwell_webhook",
    tag = "signatures",
    params(WebhookQuery, ("x-webhook-token" = Option<String>, Header, description = "`SIGNWELL_WEBHOOK_SECRET`")),
    request_body(content = Object, description = "The event, as SignWell sends it: `event` (`type`, `time`, `hash`) and `data.object.id`"),
    responses(
        (status = 200, description = "Taken (an unknown document or event changes nothing)", body = WebhookOk),
        (status = 400, description = "Not an event, or no document id", body = ErrorBody),
        (status = 401, description = "Neither the token nor the hash checks out", body = ErrorBody),
    )
)]
pub async fn receive(
    State(state): State<AppState>,
    Query(query): Query<WebhookQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<WebhookOk>> {
    let invalid = || ApiError::BadRequest("Invalid payload".into());
    let payload: Value = serde_json::from_slice(&body).map_err(|_| invalid())?;
    let (Some(event), Some(data)) = (payload.get("event"), payload.get("data")) else {
        return Err(invalid());
    };
    if !is_object(event) || !is_object(data) {
        return Err(invalid());
    }
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let time = event
        .get("time")
        .and_then(Value::as_f64)
        .filter(|t| t.is_finite())
        .unwrap_or(0.0);
    let hash = event
        .get("hash")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let token = query.token.or_else(|| {
        headers
            .get("x-webhook-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    });
    if !authorized(&state, token.as_deref(), hash, kind, time) {
        return Err(ApiError::Unauthorized);
    }
    let document_id = data
        .get("object")
        .and_then(|o| o.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if document_id.is_empty() {
        return Err(ApiError::BadRequest("Missing document id".into()));
    }
    apply_event(&state, document_id, kind).await?;
    Ok(Json(WebhookOk { ok: true }))
}

/// JS `typeof x === 'object' && x`: objects and arrays.
const fn is_object(v: &Value) -> bool {
    matches!(v, Value::Object(_) | Value::Array(_))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The event's hash `SignWell` signs: hex HMAC-SHA256 of `<type>@<time>`.
pub fn event_hash(webhook_id: &str, kind: &str, time: f64) -> String {
    use std::fmt::Write as _;
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(webhook_id.as_bytes())
        .expect("HMAC takes any key");
    mac.update(format!("{kind}@{}", num_str(time)).as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn authorized(state: &AppState, token: Option<&str>, hash: &str, kind: &str, time: f64) -> bool {
    let keys = &state.signwell_webhook;
    if let (Some(secret), Some(token)) = (keys.secret.as_deref(), token)
        && constant_time_eq(secret.as_bytes(), token.as_bytes())
    {
        return true;
    }
    match keys.id.as_deref() {
        Some(id) if !hash.is_empty() => {
            constant_time_eq(event_hash(id, kind, time).as_bytes(), hash.as_bytes())
        }
        _ => false,
    }
}

/// `mapSignWellEventToStatus`.
pub fn status_for_event(kind: &str) -> Option<&'static str> {
    match kind.trim().to_lowercase().replace('.', "_").as_str() {
        "document_viewed" => Some("viewed"),
        "document_signed" | "document_completed" | "completed" | "signed" => Some("signed"),
        "document_declined" | "declined" => Some("declined"),
        "document_expired" | "document_canceled" | "document_cancelled" | "expired"
        | "canceled" | "cancelled" => Some("expired"),
        "document_sent" | "sent" => Some("sent"),
        _ => None,
    }
}

/// `parseSignatureStatus`: an unknown stored status reads as `sent`.
fn parse_status(status: &str) -> &str {
    match status {
        "draft" | "sent" | "viewed" | "signed" | "declined" | "expired" => status,
        _ => "sent",
    }
}

/// `applySignWellWebhookEvent`.
async fn apply_event(state: &AppState, document_id: &str, kind: &str) -> ApiResult<()> {
    let document_id = document_id.trim();
    let Some(next) = status_for_event(kind) else {
        return Ok(());
    };
    if document_id.is_empty() {
        return Ok(());
    }
    let row: Option<(i64, String, i64, Option<i64>, String)> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), CAST(status AS CHAR), CAST(company_id AS SIGNED),
                CAST(customer_id AS SIGNED), filename
           FROM signature_requests
          WHERE provider = 'signwell' AND provider_document_id = ?
          LIMIT 1",
    )
    .bind(document_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((id, status, company_id, customer_id, filename)) = row else {
        return Ok(());
    };
    let current = parse_status(&status);
    if !crate::routes::customers::signatures::can_advance(current, next) {
        return Ok(());
    }
    let changed = sqlx::query("UPDATE signature_requests SET status = ? WHERE id = ?")
        .bind(next)
        .bind(id)
        .execute(&state.pool)
        .await?
        .rows_affected()
        > 0;
    if !changed {
        return Ok(());
    }
    let company_id = i32::try_from(company_id).unwrap_or_default();
    if next != current
        && let Some(event) = NoticeEvent::of(next)
    {
        notify(state, id, company_id, event).await;
    }
    // A signed contract from a quote is a sale: the quote is sold, its deal won.
    if next == "signed"
        && let Err(err) = quote_pricing::sell_quote_for_signed_contract(
            &state.pool,
            company_id,
            customer_id,
            &filename,
        )
        .await
    {
        tracing::error!(error = %err, request = id, "Failed to mark the quote sold after a signed contract");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct NoticeEvent {
    kind: &'static str,
    verb: &'static str,
    title: &'static str,
    status: &'static str,
}

impl NoticeEvent {
    const fn of(status: &str) -> Option<Self> {
        match status.as_bytes() {
            b"viewed" => Some(Self {
                kind: "signature_viewed",
                verb: "opened",
                title: "Your Document Has Been Viewed",
                status: "Viewed",
            }),
            b"signed" => Some(Self {
                kind: "signature_signed",
                verb: "signed",
                title: "Your Document Has Been Signed",
                status: "Signed",
            }),
            b"declined" => Some(Self {
                kind: "signature_declined",
                verb: "declined",
                title: "Your Document Was Declined",
                status: "Declined",
            }),
            _ => None,
        }
    }

    fn explain(self, signer: &str) -> String {
        match self.kind {
            "signature_viewed" => format!(
                "This is just a quick note that {signer} has viewed your document. You'll receive another message once it has been signed."
            ),
            "signature_signed" => {
                format!(
                    "Good news: {signer} has signed your document. The signed copy is saved in Granite Manager."
                )
            }
            _ => format!(
                "{signer} has declined to sign your document. You can reach out to them, or send a new version."
            ),
        }
    }
}

#[derive(sqlx::FromRow)]
struct NoticeRow {
    sender_email: Option<String>,
    sender_name: Option<String>,
    company_name: Option<String>,
    company_logo_url: Option<String>,
    customer_id: Option<i64>,
    deal_id: Option<i64>,
    filename: String,
    signer_name: String,
    status: String,
    provider: String,
    provider_document_id: Option<String>,
    completed_object_key: Option<String>,
}

/// A contract sent from a quote belongs to that quote's deal.
async fn quote_deal_id(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: Option<i64>,
    filename: &str,
) -> ApiResult<Option<i64>> {
    let Some(customer_id) = customer_id else {
        return Ok(None);
    };
    let quotes: Vec<(Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT quote_name, CAST(deal_id AS SIGNED) FROM quotes
          WHERE company_id = ? AND customer_id = ? AND deleted_at IS NULL
          ORDER BY updated_at DESC, id DESC",
    )
    .bind(company_id)
    .bind(customer_id)
    .fetch_all(pool)
    .await?;
    let forms: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM quote_forms
          WHERE company_id = ? AND deleted_at IS NULL
            AND JSON_UNQUOTE(JSON_EXTRACT(config_json, '$.kind')) = 'contract'
          ORDER BY sequence, name",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(
        quote_pricing::quote_for_contract_file(filename, &forms, &quotes, |q| q.0.as_deref())
            .and_then(|q| q.1),
    )
}

/// `notifySignatureEvent`: the sender hears that the signer opened, signed
/// or declined the document, by email and in the app. A failure loses only
/// that notice, never the status.
#[allow(clippy::too_many_lines)]
async fn notify(state: &AppState, request_id: i64, company_id: i32, event: NoticeEvent) {
    let row = async {
        let row: Option<NoticeRow> = sqlx::query_as(
            "SELECT CAST(sr.created_by_user_id AS SIGNED) AS created_by_user_id, u.email AS sender_email,
                    u.name AS sender_name, c.name AS company_name, c.logo_url AS company_logo_url,
                    CAST(sr.customer_id AS SIGNED) AS customer_id,
                    CAST(COALESCE(sr.deal_id, e.deal_id) AS SIGNED) AS deal_id, sr.filename, sr.signer_name,
                    CAST(sr.status AS CHAR) AS status, sr.provider, sr.provider_document_id,
                    nd.completed_object_key
               FROM signature_requests sr
               LEFT JOIN emails e ON e.id = sr.email_id
               LEFT JOIN native_signature_documents nd
                      ON nd.request_id = sr.id AND nd.company_id = sr.company_id
               LEFT JOIN users u ON u.id = sr.created_by_user_id
               LEFT JOIN company c ON c.id = sr.company_id
              WHERE sr.id = ? AND sr.company_id = ?
              LIMIT 1",
        )
        .bind(request_id)
        .bind(company_id)
        .fetch_optional(&state.pool)
        .await?;
        let Some(row) = row else { return ApiResult::Ok(None) };
        let deal_id = match row.deal_id {
            Some(id) => Some(id),
            None => quote_deal_id(&state.pool, company_id, row.customer_id, &row.filename).await?,
        };
        Ok(Some((row, deal_id)))
    }
    .await;
    let (row, deal_id) = match row {
        Ok(Some(found)) => found,
        Ok(None) => return,
        Err(err) => {
            tracing::error!(error = %err, request = request_id, "Failed to notify that a signature request was {}", event.verb);
            return;
        }
    };
    let signer = Some(row.signer_name.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("The customer");
    let filename = strip_pdf(&row.filename);
    let message = format!("{signer} {} {filename}", event.verb);
    let path = deal_id.map_or_else(
        || {
            row.customer_id
                .map(|id| format!("/employee/customers/info/{id}/documents"))
        },
        |id| Some(format!("/employee/deals/edit/{id}/project")),
    );

    if let Some(email) = row
        .sender_email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        let link = path
            .as_ref()
            .map(|p| format!("{}{p}", state.signature_app_origin));
        let sender_name = row
            .sender_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or(email);
        let branding = Branding {
            company_name: row
                .company_name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .unwrap_or(sender_name)
                .to_string(),
            logo_url: row.company_logo_url.clone(),
            sender_name: sender_name.to_string(),
        };
        let html = status_html(
            &branding,
            event.title,
            &filename,
            &event.explain(signer),
            signer,
            event.status,
            link.as_deref()
                .map(|href| (href, "Open in Granite Manager")),
        );
        let outgoing = OutgoingEmail {
            from_name: None,
            from_address: "noreply@granite-manager.com".into(),
            to: vec![email.to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: message.clone(),
            body: html,
            signature: None,
            reply_to: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            attachments: signed_copy(state, request_id, company_id, &row)
                .await
                .into_iter()
                .collect(),
            transactional: true,
        };
        if let Err(err) = state.webhooks.send_email(&outgoing).await {
            tracing::error!(error = ?err, request = request_id, "Failed to email that a signature request was {}", event.verb);
        }
    }
}

/// The signed PDF to attach to the "signed" notice: the completed copy at
/// `SignWell`, or the native one in the signature bucket. `None` while the
/// request is not signed, or when the copy cannot be reached.
async fn signed_copy(
    state: &AppState,
    request_id: i64,
    company_id: i32,
    row: &NoticeRow,
) -> Option<OutgoingAttachment> {
    if row.status != "signed" {
        return None;
    }
    let filename = crate::routes::signatures::signed_pdf_filename(&row.filename);
    let url = if row.provider == "native" {
        let key = row.completed_object_key.as_deref()?;
        let bucket = state.signature_bucket.as_deref()?;
        state
            .storage
            .presign_get_in(Some(bucket), key, "application/pdf", Some(&filename))
            .await
            .ok()?
    } else {
        let document_id = row
            .provider_document_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())?;
        crate::routes::signatures::signwell_completed_pdf_url(state, company_id, document_id)
            .await?
    };
    tracing::debug!(request = request_id, "Attaching the signed copy");
    Some(OutgoingAttachment {
        filename,
        content_type: "application/pdf".into(),
        url,
    })
}

/// Tells the sender a request moved to `status` (`viewed`, `signed`,
/// `declined`); other statuses send nothing. Shared with native signing.
pub async fn notify_status(state: &AppState, request_id: i64, company_id: i32, status: &str) {
    if let Some(event) = NoticeEvent::of(status) {
        notify(state, request_id, company_id, event).await;
    }
}

/// The name without a trailing `.pdf` (any case).
fn strip_pdf(name: &str) -> String {
    let len = name.len();
    if len >= 4 && name.is_char_boundary(len - 4) && name[len - 4..].eq_ignore_ascii_case(".pdf") {
        name[..len - 4].to_string()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_map_to_statuses() {
        assert_eq!(status_for_event("document_completed"), Some("signed"));
        assert_eq!(status_for_event(" Document.Viewed "), Some("viewed"));
        assert_eq!(status_for_event("document_canceled"), Some("expired"));
        assert_eq!(status_for_event("document_created"), None);
    }

    #[test]
    fn the_hash_signs_type_and_time() {
        let a = event_hash("webhook-id", "document_viewed", 1_700_000_000.0);
        assert_eq!(a.len(), 64);
        assert_ne!(
            a,
            event_hash("webhook-id", "document_viewed", 1_700_000_001.0)
        );
        assert_eq!(strip_pdf("Contract - Kitchen.PDF"), "Contract - Kitchen");
    }
}
