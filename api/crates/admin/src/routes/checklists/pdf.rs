//! `GET /v1/checklists/{id}/pdf`: a signed checklist as a PDF.
//!
//! Drawn exactly like the Remix `api.checklist-pdf.$id` route did with
//! pdf-lib: the company logo, the checks, the comments wrapped at the same
//! width, and the customer's signature.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::IntoParams;
use crate::pdf::{Font, Image, PAGE_HEIGHT, PAGE_WIDTH, Page};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use lambda_http::tracing;
use serde::Deserialize;
use std::time::Duration;

/// The onboarding tour's sample checklist.
const SAMPLE_ID: i64 = 990_000_051;

#[derive(sqlx::FromRow)]
struct ChecklistRow {
    id: i64,
    customer_name: String,
    installation_address: String,
    logo_url: Option<String>,
    material_correct: bool,
    seams_satisfaction: bool,
    appliances_fit: bool,
    backsplashes_correct: bool,
    edges_correct: bool,
    holes_drilled: bool,
    cleanup_completed: bool,
    comments: Option<String>,
    signature: String,
}

fn sample() -> ChecklistRow {
    ChecklistRow {
        id: SAMPLE_ID,
        customer_name: "Onboarding Sample Checklist Customer".into(),
        installation_address: "456 Install Avenue, Indianapolis, IN 46202".into(),
        logo_url: None,
        material_correct: true,
        seams_satisfaction: true,
        appliances_fit: true,
        backsplashes_correct: true,
        edges_correct: true,
        holes_drilled: true,
        cleanup_completed: true,
        comments: Some("Sample post-install checklist for onboarding. All items passed.".into()),
        signature: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==".into(),
    }
}

/// pdf-lib embeds only PNG and JPEG.
fn png_or_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG") || bytes.starts_with(&[0xFF, 0xD8])
}

async fn logo(state: &AppState, url: Option<&str>) -> Option<Image> {
    let url = url
        .map(str::trim)
        .filter(|url| !url.is_empty() && *url != "undefined")?;
    let response = state
        .telephony_http
        .get(url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let bytes = response.bytes().await.ok()?;
    png_or_jpeg(&bytes).then(|| Image::decode(&bytes)).flatten()
}

/// pdf-lib's line wrapping of the CRM: words split on single spaces.
pub fn wrap(text: &str, font: Font, size: f32, max_width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if font.width(&candidate, size) > max_width && !current.is_empty() {
            lines.push(std::mem::replace(&mut current, word.to_string()));
        } else {
            current = candidate;
        }
    }
    lines.push(current);
    lines
}

struct Writer {
    page: Page,
    y: f32,
}

impl Writer {
    fn text(&mut self, text: &str, bold: bool, size: f32, indent: f32) {
        let font = if bold { Font::Bold } else { Font::Regular };
        self.page.text(text, 50.0 + indent, self.y, size, font);
        self.y -= size + 6.0;
    }

    fn checkbox(&mut self, label: &str, checked: bool) {
        let (x, y) = (60.0, self.y - 1.0);
        self.page.rectangle(x, y, 10.0, 10.0, 1.0);
        if checked {
            self.page.line((x + 1.0, y + 1.0), (x + 9.0, y + 9.0), 1.0);
            self.page.line((x + 9.0, y + 1.0), (x + 1.0, y + 9.0), 1.0);
        }
        self.page.text(label, x + 15.0, self.y, 12.0, Font::Regular);
        self.y -= 18.0;
    }
}

#[allow(clippy::cast_precision_loss)]
fn draw(checklist: &ChecklistRow, logo: Option<Image>) -> Vec<u8> {
    let mut writer = Writer {
        page: Page::new(),
        y: PAGE_HEIGHT - 60.0,
    };
    if let Some(logo) = logo {
        let (width, height) = (logo.width as f32 * 0.125, logo.height as f32 * 0.125);
        writer.page.image(
            logo,
            (PAGE_WIDTH - width) / 2.0,
            PAGE_HEIGHT - height - 20.0,
            width,
            height,
        );
        writer.y -= height + 20.0;
    }
    writer.text("Post-installation check list", true, 16.0, 0.0);
    writer.text(
        &format!("Customer Name: {}", checklist.customer_name),
        false,
        12.0,
        0.0,
    );
    writer.text(
        &format!("Installation Address: {}", checklist.installation_address),
        false,
        12.0,
        0.0,
    );
    writer.text("Check all that apply:", true, 12.0, 0.0);
    for (label, checked) in [
        ("Material is correct", checklist.material_correct),
        ("Seams meet my satisfaction", checklist.seams_satisfaction),
        ("Appliances fit properly", checklist.appliances_fit),
        (
            "Backsplashes placed correctly",
            checklist.backsplashes_correct,
        ),
        ("Edges and corners are correct", checklist.edges_correct),
        ("Holes for fixtures are drilled", checklist.holes_drilled),
        ("Clean up completed", checklist.cleanup_completed),
    ] {
        writer.checkbox(label, checked);
    }
    writer.text("Comments:", true, 12.0, 0.0);
    for paragraph in checklist
        .comments
        .as_deref()
        .unwrap_or("")
        .split(['\n', '\r'])
    {
        for line in wrap(paragraph, Font::Regular, 12.0, PAGE_WIDTH - 110.0) {
            writer.text(&line, false, 12.0, 10.0);
        }
    }
    writer.y -= 10.0;
    writer.text("Signature:", true, 12.0, 0.0);
    if checklist.signature.starts_with("data:image") {
        let png = checklist
            .signature
            .split_once(',')
            .and_then(|(_, data)| STANDARD.decode(data).ok())
            .filter(|bytes| bytes.starts_with(b"\x89PNG"))
            .and_then(|bytes| Image::decode(&bytes));
        if let Some(image) = png {
            let (width, height) = (image.width as f32, image.height as f32);
            writer
                .page
                .image(image, 60.0, writer.y - height + 12.0, width, height);
            writer.y -= height + 6.0;
        } else {
            writer.text("(signature unreadable)", false, 12.0, 10.0);
        }
    } else {
        let text = if checklist.signature.is_empty() {
            "N/A"
        } else {
            &checklist.signature
        };
        writer.text(text, false, 12.0, 10.0);
    }
    writer.page.finish()
}

/// Query of `GET /v1/checklists/{id}/pdf`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PdfQuery {
    /// The onboarding tour is open: its sample checklist can be opened.
    #[serde(default)]
    pub samples: bool,
}

/// Get a checklist as a PDF
///
/// The checklist with its checks, comments and the customer's signature,
/// under the company logo.
#[crate::openapi::path(
    get,
    path = "/v1/checklists/{id}/pdf",
    operation_id = "get_checklist_pdf",
    tag = "checklists",
    params(("id" = i64, Path, description = "Checklist id"), PdfQuery),
    responses(
        (status = 200, description = "The PDF", content_type = "application/pdf", body = Vec<u8>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such checklist in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn pdf(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
    Query(query): Query<PdfQuery>,
) -> ApiResult<impl IntoResponse> {
    let checklist = if id == SAMPLE_ID {
        if !query.samples {
            return Err(ApiError::NotFound);
        }
        sample()
    } else {
        let row: Option<ChecklistRow> = sqlx::query_as(
            "SELECT CAST(c.id AS SIGNED) AS id, c.customer_name, c.installation_address, co.logo_url,
                    c.material_correct <> 0 AS material_correct, c.seams_satisfaction <> 0 AS seams_satisfaction,
                    c.appliances_fit <> 0 AS appliances_fit, c.backsplashes_correct <> 0 AS backsplashes_correct,
                    c.edges_correct <> 0 AS edges_correct, c.holes_drilled <> 0 AS holes_drilled,
                    c.cleanup_completed <> 0 AS cleanup_completed, c.comments, c.signature
               FROM checklists c JOIN company co ON co.id = c.company_id
              WHERE c.id = ? AND c.company_id = ?",
        )
        .bind(id)
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?;
        row.ok_or(ApiError::NotFound)?
    };
    let logo = logo(&state, checklist.logo_url.as_deref()).await;
    if checklist.logo_url.is_some() && logo.is_none() {
        tracing::info!(checklist = checklist.id, "checklist PDF without its logo");
    }
    let bytes = draw(&checklist, logo);
    let safe: String = checklist
        .customer_name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();
    Ok((
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"checklist-{safe}-{}.pdf\"", checklist.id),
            ),
        ],
        bytes,
    ))
}

#[cfg(test)]
mod tests {
    use super::{Font, wrap};

    #[test]
    fn comments_wrap_on_spaces() {
        let text = "word ".repeat(40);
        let lines = wrap(text.trim_end(), Font::Regular, 12.0, 485.28);
        assert!(lines.len() > 1);
        assert!(
            lines
                .iter()
                .all(|line| Font::Regular.width(line, 12.0) <= 485.28)
        );
        assert_eq!(wrap("", Font::Regular, 12.0, 100.0), [""]);
    }
}
