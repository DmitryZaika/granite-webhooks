//! Signed documents, opened in a browser tab.
//!
//! - `GET /v1/signatures/{id}/document` → a natively signed (or sent)
//!   document, from the private signature bucket
//! - `GET /v1/signature-requests/{id}/signed-pdf` → a `SignWell` document's
//!   signed PDF, named "<document> signed.pdf" ("<quote> signed.pdf" for a
//!   quote's contract)
//!
//! Replaces the Remix `api.signatures.$requestId.document` and
//! `api.signature-requests.$requestId.signed-pdf` loaders.

use crate::auth::EmployeeUser;
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use lambda_http::tracing;
use lopdf::{Dictionary, Document, Object, StringFormat};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(native_document))
        .routes(routes!(signed_pdf))
}

/// Biggest PDF answered in the response body: Lambda responses stop at
/// 6 MB, base64 included. Bigger ones go through a presigned link.
pub const MAX_INLINE_BYTES: usize = 4 * 1024 * 1024;
/// Where a big signed `SignWell` PDF is put for its presigned link (the
/// bucket's lifecycle rule may expire this folder after a day).
const SIGNED_COPIES: &str = "signed-documents";

const UNAVAILABLE: &str = "The document is temporarily unavailable. Please try again.";

/// Plain text for the tab the document was opened in.
fn text(status: StatusCode, message: &str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        message.to_string(),
    )
        .into_response()
}

fn redirect(location: &str) -> Response {
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, location.to_string()),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
    )
        .into_response()
}

fn pdf(bytes: Vec<u8>, filename: &str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (
                header::CONTENT_DISPOSITION,
                inline_pdf_disposition(filename),
            ),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        bytes,
    )
        .into_response()
}

/// `Content-Disposition` with an ASCII fallback and the full UTF-8 name.
pub fn inline_pdf_disposition(filename: &str) -> String {
    let ascii: String = filename
        .chars()
        .map(|c| {
            if (' '..='~').contains(&c) && !matches!(c, '"' | '\\') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded =
        percent_encoding::utf8_percent_encode(filename, percent_encoding::NON_ALPHANUMERIC);
    format!("inline; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

/// The name a signed PDF opens under: the document's own name plus
/// "signed" ("John Clay.pdf" becomes "John Clay signed.pdf").
pub fn signed_pdf_filename(filename: &str) -> String {
    let trimmed = filename.trim();
    let base = if trimmed.to_lowercase().ends_with(".pdf") {
        &trimmed[..trimmed.len() - 4]
    } else {
        trimmed
    };
    let cleaned: String = base
        .chars()
        .map(|c| {
            if matches!(
                c,
                '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\r' | '\n'
            ) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let base = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "{} signed.pdf",
        if base.is_empty() { "Document" } else { &base }
    )
}

/// The name a signed copy goes by: [`signed_pdf_filename`], except that a
/// contract sent from a quote (`<contract form> - <quote name>.pdf`) drops
/// the form and keeps only the quote's name ("Kitchen signed.pdf").
pub async fn signed_copy_filename(
    pool: &sqlx::MySqlPool,
    company_id: i32,
    customer_id: Option<i64>,
    filename: &str,
) -> String {
    let quote_name = async {
        let Some(customer_id) = customer_id else {
            return Ok(None);
        };
        let quotes: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT quote_name FROM quotes
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
        let quote = crate::quote_pricing::quote_for_contract_file(
            filename,
            &forms,
            &quotes,
            Option::as_deref,
        );
        Ok::<_, sqlx::Error>(quote.map(|name| {
            name.as_deref()
                .filter(|n| !n.is_empty())
                .unwrap_or("Quote")
                .to_string()
        }))
    }
    .await
    .unwrap_or_else(|err| {
        tracing::error!(?err, "signed copy quote lookup failed");
        None
    });
    signed_pdf_filename(quote_name.as_deref().unwrap_or(filename))
}

/// Titles the PDF, so the viewer's tab shows the file's name. An untitled
/// PDF still opens, so a PDF that cannot be edited is returned as it is.
pub fn with_title(bytes: Vec<u8>, title: &str) -> Vec<u8> {
    let titled = || -> Option<Vec<u8>> {
        let mut document = Document::load_mem(&bytes).ok()?;
        if document.is_encrypted() {
            return None;
        }
        let utf16: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain(title.encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        let title = Object::String(utf16, StringFormat::Hexadecimal);
        let info_id = if let Ok(id) = document.trailer.get(b"Info").and_then(Object::as_reference) {
            id
        } else {
            let id = document.add_object(Dictionary::new());
            document.trailer.set("Info", Object::Reference(id));
            id
        };
        document
            .get_object_mut(info_id)
            .ok()?
            .as_dict_mut()
            .ok()?
            .set("Title", title);
        let catalog = document.catalog_mut().ok()?;
        let mut preferences = match catalog.get(b"ViewerPreferences") {
            Ok(Object::Dictionary(dict)) => dict.clone(),
            _ => Dictionary::new(),
        };
        preferences.set("DisplayDocTitle", true);
        catalog.set("ViewerPreferences", Object::Dictionary(preferences));
        let mut out = Vec::new();
        document.save_to(&mut out).ok()?;
        Some(out)
    };
    titled().unwrap_or(bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DocumentQuery {
    /// Opened from this customer's documents: anyone in the company may
    /// open the customer's documents. Otherwise only the sender or an admin.
    #[serde(default, alias = "customerId")]
    pub customer_id: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct NativeRow {
    company_id: i64,
    document_id: String,
    filename: String,
    status: String,
    prepared_object_key: String,
    prepared_sha256: String,
    completed_object_key: Option<String>,
    completed_sha256: Option<String>,
}

/// Open a natively signed document
///
/// The document as sent, or signed once the customer signed it, from the
/// private signature bucket; one over 4 MB redirects to a presigned link.
/// For a browser tab: errors are plain text.
#[utoipa::path(
    get,
    path = "/v1/signatures/{id}/document",
    operation_id = "open_native_signature_document",
    tag = "customers",
    params(("id" = i64, Path, description = "Signature request id"), DocumentQuery),
    responses(
        (status = 200, description = "The PDF", content_type = "application/pdf"),
        (status = 302, description = "Over 4 MB: redirect to a presigned link"),
        (status = 401, description = "No valid session, or not an employee", content_type = "text/plain"),
        (status = 404, description = "No such document the caller may open", content_type = "text/plain"),
        (status = 503, description = "The file cannot be read right now", content_type = "text/plain"),
    ),
    security(("session_cookie" = []))
)]
pub async fn native_document(
    State(state): State<AppState>,
    user: Result<EmployeeUser, ApiError>,
    Path(id): Path<i64>,
    Query(query): Query<DocumentQuery>,
) -> Result<Response, ApiError> {
    let Ok(EmployeeUser(user)) = user else {
        return Ok(text(
            StatusCode::UNAUTHORIZED,
            "Please sign in to view this document.",
        ));
    };
    let unavailable = || text(StatusCode::NOT_FOUND, "Document unavailable");
    if id <= 0 || query.customer_id.is_some_and(|c| c <= 0) {
        return Ok(unavailable());
    }
    let is_admin = user.is_admin || user.is_superuser;
    let sql = format!(
        "SELECT CAST(nd.company_id AS SIGNED) AS company_id, nd.document_id, sr.filename,
                CAST(sr.status AS CHAR) AS status, nd.prepared_object_key, nd.prepared_sha256,
                nd.completed_object_key, nd.completed_sha256
           FROM signature_requests sr
           INNER JOIN native_signature_documents nd ON nd.request_id = sr.id AND nd.company_id = sr.company_id
           {}
          WHERE sr.id = ? AND sr.company_id = ? AND sr.provider = 'native'
            AND sr.provider_document_id = nd.document_id {}
          LIMIT 1",
        if query.customer_id.is_some() {
            "INNER JOIN customers c ON c.id = sr.customer_id AND c.company_id = sr.company_id AND c.deleted_at IS NULL"
        } else {
            ""
        },
        if query.customer_id.is_some() {
            "AND sr.customer_id = ?"
        } else if is_admin {
            ""
        } else {
            "AND sr.created_by_user_id = ?"
        },
    );
    let mut select = sqlx::query_as::<_, NativeRow>(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .bind(user.company_id);
    if let Some(customer_id) = query.customer_id {
        select = select.bind(customer_id);
    } else if !is_admin {
        select = select.bind(user.id);
    }
    let Some(row) = select.fetch_optional(&state.pool).await? else {
        return Ok(unavailable());
    };
    let (key, hash) = if row.status == "signed" {
        (
            row.completed_object_key.clone(),
            row.completed_sha256.clone(),
        )
    } else {
        (
            Some(row.prepared_object_key.clone()),
            Some(row.prepared_sha256.clone()),
        )
    };
    let (Some(key), Some(hash)) = (key, hash) else {
        return Ok(text(
            StatusCode::SERVICE_UNAVAILABLE,
            "The signed document is temporarily unavailable. Please try again.",
        ));
    };
    // Only this document's own objects, and only bytes that match their hash.
    let prefix = format!("{}/signatures/{}/", row.company_id, row.document_id);
    let valid_hash = hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !valid_hash
        || (key != format!("{prefix}prepared-{hash}.pdf")
            && key != format!("{prefix}completed-{hash}.pdf"))
    {
        return Ok(text(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE));
    }
    let Some(bucket) = state.signature_bucket.as_deref() else {
        return Ok(text(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE));
    };
    let object = match state.storage.get_in(bucket, &key).await {
        Ok(Some(object)) if sha256_hex(&object.bytes) == hash => object,
        Ok(_) => return Ok(text(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE)),
        Err(err) => {
            tracing::error!(?err, id, "native signature document read failed");
            return Ok(text(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE));
        }
    };
    if object.bytes.len() > MAX_INLINE_BYTES {
        let link = state
            .storage
            .presign_get_in(Some(bucket), &key, "application/pdf", Some(&row.filename))
            .await;
        return Ok(match link {
            Ok(link) => redirect(&link),
            Err(err) => {
                tracing::error!(?err, id, "native signature document presign failed");
                text(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE)
            }
        });
    }
    Ok(pdf(object.bytes, &row.filename))
}

#[derive(sqlx::FromRow)]
struct RequestRow {
    filename: String,
    file_url: String,
    status: String,
    provider: String,
    provider_document_id: Option<String>,
    customer_id: Option<i64>,
}

/// The URL of a `SignWell` document's signed PDF.
pub(crate) async fn signwell_completed_pdf_url(
    state: &AppState,
    company_id: i32,
    document_id: &str,
) -> Option<String> {
    let company_key: Option<Option<String>> =
        sqlx::query_scalar("SELECT signwell_api_key FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(&state.pool)
            .await
            .ok()?;
    let key = company_key
        .flatten()
        .or_else(|| std::env::var("SIGNWELL_API_KEY").ok())
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())?;
    let encoded = percent_encoding::utf8_percent_encode(
        document_id.trim(),
        percent_encoding::NON_ALPHANUMERIC,
    );
    let payload: serde_json::Value = state
        .telephony_http
        .get(format!(
            "{}/documents/{encoded}/completed_pdf?url_only=true",
            state.signwell_base
        ))
        .header("X-Api-Key", key)
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    // HTTPS only, unless the API itself is a local stand-in on plain HTTP.
    let plain_http_ok = state.signwell_base.starts_with("http://");
    payload["file_url"]
        .as_str()
        .filter(|url| url.starts_with("https://") || (plain_http_ok && url.starts_with("http://")))
        .map(str::to_string)
}

/// Open a signed `SignWell` document
///
/// The signed PDF, titled and named "<document> signed.pdf"; the original
/// file while it is not signed. One over 4 MB is stored in the bucket and
/// redirects to a presigned link. For a browser tab: errors are plain text.
#[utoipa::path(
    get,
    path = "/v1/signature-requests/{id}/signed-pdf",
    operation_id = "open_signed_pdf",
    tag = "customers",
    params(("id" = i64, Path, description = "Signature request id")),
    responses(
        (status = 200, description = "The PDF", content_type = "application/pdf"),
        (status = 302, description = "Over 4 MB: redirect to a presigned link"),
        (status = 401, description = "No valid session, or not an employee", content_type = "text/plain"),
        (status = 404, description = "No such request in the caller's company", content_type = "text/plain"),
        (status = 502, description = "The document could not be downloaded", content_type = "text/plain"),
    ),
    security(("session_cookie" = []))
)]
pub async fn signed_pdf(
    State(state): State<AppState>,
    user: Result<EmployeeUser, ApiError>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let Ok(EmployeeUser(user)) = user else {
        return Ok(text(
            StatusCode::UNAUTHORIZED,
            "Please sign in to view this document.",
        ));
    };
    let row: Option<RequestRow> = sqlx::query_as(
        "SELECT filename, file_url, CAST(status AS CHAR) AS status, provider, provider_document_id,
                CAST(customer_id AS SIGNED) AS customer_id
           FROM signature_requests WHERE id = ? AND company_id = ? LIMIT 1",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some(row) = row.filter(|row| row.provider != "native") else {
        return Ok(text(StatusCode::NOT_FOUND, "Not found"));
    };
    let failed = || {
        text(
            StatusCode::BAD_GATEWAY,
            "Could not load the signed document",
        )
    };
    let document_id = row
        .provider_document_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let completed = match document_id {
        Some(document_id) if row.status == "signed" => {
            signwell_completed_pdf_url(&state, user.company_id, document_id).await
        }
        _ => None,
    };
    let bytes = if let Some(url) = completed {
        let response = state
            .telephony_http
            .get(&url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status);
        match response {
            Ok(response) => match response.bytes().await {
                Ok(bytes) => bytes.to_vec(),
                Err(err) => {
                    tracing::error!(?err, id, "signed document download failed");
                    return Ok(failed());
                }
            },
            Err(err) => {
                tracing::error!(?err, id, "signed document download failed");
                return Ok(failed());
            }
        }
    } else {
        match state.storage.get_url(&row.file_url).await {
            Ok(Some(object)) => object.bytes,
            Ok(None) => return Ok(failed()),
            Err(err) => {
                tracing::error!(?err, id, "signature request file read failed");
                return Ok(failed());
            }
        }
    };
    let filename =
        signed_copy_filename(&state.pool, user.company_id, row.customer_id, &row.filename).await;
    let title = filename
        .strip_suffix(".pdf")
        .unwrap_or(&filename)
        .to_string();
    let bytes = tokio::task::spawn_blocking(move || with_title(bytes, &title))
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    if bytes.len() > MAX_INLINE_BYTES {
        let key = format!(
            "{SIGNED_COPIES}/{}/{id}-{}.pdf",
            user.company_id,
            sha256_hex(&bytes)
        );
        state.storage.put(&key, bytes, "application/pdf").await?;
        let link = state
            .storage
            .presign_get_as(&key, "application/pdf", Some(&filename))
            .await?;
        return Ok(redirect(&link));
    }
    Ok(pdf(bytes, &filename))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_names_and_dispositions_match_the_crm() {
        assert_eq!(signed_pdf_filename("John Clay.pdf"), "John Clay signed.pdf");
        assert_eq!(signed_pdf_filename("  a/b:c  .PDF "), "a b c signed.pdf");
        assert_eq!(signed_pdf_filename(" .pdf"), "Document signed.pdf");
        assert_eq!(
            inline_pdf_disposition("Café signed.pdf"),
            "inline; filename=\"Caf_ signed.pdf\"; filename*=UTF-8''Caf%C3%A9%20signed%2Epdf"
        );
    }
}
