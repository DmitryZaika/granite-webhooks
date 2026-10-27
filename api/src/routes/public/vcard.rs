//! `GET /v1/public/vcard`: a contact card file built from the query, for
//! in-app browsers (Telegram) that cannot save a card made in the page.
//! Replaces the Remix `api.vcard` route.

use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::IntoParams;
use axum::extract::Query;
use axum::http::header;
use axum::response::IntoResponse;
use serde::Deserialize;

/// Query of `GET /v1/public/vcard`; at least one contact field.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct VCardQuery {
    /// Full name.
    pub name: Option<String>,
    /// Phone number.
    pub phone: Option<String>,
    /// Email address.
    pub email: Option<String>,
    /// Company.
    pub company: Option<String>,
    /// Street address, one line.
    pub address: Option<String>,
    /// Download name; `.vcf` is added. Defaults to the name.
    pub filename: Option<String>,
}

/// Trimmed, with `null` / `undefined` treated as empty.
fn clean(value: Option<&String>) -> &str {
    let value = value.map_or("", |value| value.trim());
    if value.eq_ignore_ascii_case("null") || value.eq_ignore_ascii_case("undefined") {
        ""
    } else {
        value
    }
}

fn file_name(base: &str) -> String {
    let base = if base.is_empty() { "contact" } else { base };
    let mut name = String::new();
    let mut in_run = false;
    for ch in base.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == '-' {
            name.push(ch);
            in_run = false;
        } else if !in_run {
            name.push('_');
            in_run = true;
        }
    }
    if name.to_lowercase().ends_with(".vcf") {
        name
    } else {
        format!("{name}.vcf")
    }
}

/// Download a contact card
///
/// A vCard 3.0 file with the given name, phone, email, company and address.
#[crate::openapi::path(
    get,
    path = "/v1/public/vcard",
    operation_id = "download_vcard",
    tag = "public",
    params(VCardQuery),
    responses(
        (status = 200, description = "The card", content_type = "text/vcard", body = String),
        (status = 400, description = "No contact field", body = ErrorBody),
    )
)]
pub async fn vcard(Query(query): Query<VCardQuery>) -> ApiResult<impl IntoResponse> {
    let name = clean(query.name.as_ref());
    let fields = [
        ("ORG:", clean(query.company.as_ref())),
        ("TEL:", clean(query.phone.as_ref())),
        ("EMAIL:", clean(query.email.as_ref())),
    ];
    let address = clean(query.address.as_ref());
    if name.is_empty() && address.is_empty() && fields.iter().all(|(_, value)| value.is_empty()) {
        return Err(ApiError::BadRequest("Missing contact data".into()));
    }
    let mut lines = vec![
        "BEGIN:VCARD".to_string(),
        "VERSION:3.0".to_string(),
        format!("FN:{name}"),
        format!("N:{name};;;;"),
    ];
    lines.extend(
        fields
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(tag, value)| format!("{tag}{value}")),
    );
    if !address.is_empty() {
        lines.push(format!("ADR:;;{address};;;;;"));
    }
    lines.push("END:VCARD".to_string());
    let body = format!("{}\r\n", lines.join("\r\n"));
    let download = match clean(query.filename.as_ref()) {
        "" => file_name(name),
        given => file_name(given),
    };
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "text/vcard; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{download}\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    ))
}
