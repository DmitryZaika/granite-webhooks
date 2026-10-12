//! Bug reports: employees describe what went wrong (with files); the team is
//! emailed and super admins read them all.
//!
//! - `POST /v1/bug-reports` → file a report
//! - `GET  /v1/bug-reports` → every report (super admins)
//!
//! Replaces the Remix `employee.bug-report` action and the
//! `admin.superuser-dashboard.bug-reports` loader.

use crate::auth::{CurrentUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::serde_helpers::js_date;
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

/// Who hears about every bug report.
const NOTIFY: &str = "dema@granitedepotindy.com";
const NO_REPLY: &str = "noreply@granite-manager.com";
const TITLE_MAX: usize = 200;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(list, create))
}

/// Body of `POST /v1/bug-reports`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewBugReport {
    /// What happened; its first line is the title.
    pub description: String,
    /// Files uploaded to the `bug-reports` folder (`POST /v1/uploads`).
    #[serde(default)]
    pub file_urls: Vec<String>,
}

/// The filed report.
#[derive(Debug, Serialize, ToSchema)]
pub struct FiledBugReport {
    /// Report id.
    pub id: i32,
    /// The team was emailed (false when the email failed; the report is saved).
    pub notified: bool,
}

/// A report as super admins list it.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct BugReport {
    /// Report id.
    pub id: i32,
    /// Reporter.
    pub user_id: i32,
    /// Reporter's company.
    pub company_id: i32,
    /// First line of the description.
    pub title: String,
    /// What happened.
    pub description: String,
    /// When it was filed.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
    /// Reporter's name.
    pub user_name: Option<String>,
    /// Reporter's email.
    pub user_email: Option<String>,
    /// Company name.
    pub company_name: Option<String>,
    /// Files attached.
    #[sqlx(skip)]
    pub image_urls: Vec<String>,
}

/// `bugReportTitleFromDescription`: the first line, cut to 200 characters.
pub fn title_of(description: &str) -> String {
    let first = description.trim().lines().next().unwrap_or_default();
    if first.chars().count() <= TITLE_MAX {
        return first.to_string();
    }
    let cut: String = first.chars().take(TITLE_MAX - 1).collect();
    format!("{}…", cut.trim_end())
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `buildBugReportEmail`'s HTML.
fn email_html(
    title: &str,
    description: &str,
    reporter: (&str, &str),
    company: (&str, i32),
    files: &[String],
) -> String {
    let pictures = if files.is_empty() {
        String::new()
    } else {
        let mut images = String::new();
        for url in files {
            let url = escape(url);
            let _ = write!(
                images,
                "<p><a href=\"{url}\"><img src=\"{url}\" alt=\"Bug report picture\" style=\"max-width:400px;height:auto;\" /></a></p>"
            );
        }
        format!("<p><strong>Pictures:</strong></p>{images}")
    };
    format!(
        "<h2>Bug report</h2>
<p><strong>Title:</strong> {}</p>
<p><strong>Reporter:</strong> {} &lt;{}&gt;</p>
<p><strong>Company:</strong> {} (id {})</p>
<p><strong>Description:</strong></p>
<p style=\"white-space: pre-wrap;\">{}</p>
{pictures}",
        escape(title),
        escape(reporter.0),
        escape(reporter.1),
        escape(company.0),
        company.1,
        escape(description),
    )
}

/// Report a bug
///
/// Saves the report with its files and emails it to the team (replies go to
/// the reporter). When the email fails the report is still saved and
/// `notified` is false.
#[utoipa::path(
    post,
    path = "/v1/bug-reports",
    operation_id = "create_bug_report",
    tag = "support",
    request_body = NewBugReport,
    responses(
        (status = 201, description = "Filed", body = FiledBugReport),
        (status = 400, description = "No description, or a file not uploaded for bug reports", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<NewBugReport>,
) -> ApiResult<(StatusCode, Json<FiledBugReport>)> {
    let description = input.description.trim();
    if description.is_empty() {
        return Err(ApiError::BadRequest(
            "Please fill in all required fields".into(),
        ));
    }
    let files: Vec<String> = input
        .file_urls
        .into_iter()
        .filter(|url| !url.is_empty())
        .collect();
    if files.iter().any(|url| {
        !state
            .storage
            .key_for_url(url)
            .is_some_and(|key| key.starts_with("bug-reports/"))
    }) {
        return Err(ApiError::BadRequest(
            "Attach files uploaded for bug reports".into(),
        ));
    }
    let title = title_of(description);
    let mut tx = state.pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO bug_reports (user_id, company_id, title, description) VALUES (?, ?, ?, ?)",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(&title)
    .bind(description)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    for (order, url) in files.iter().enumerate() {
        sqlx::query(
            "INSERT INTO bug_report_images (bug_report_id, company_id, url, sort_order) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(user.company_id)
        .bind(url)
        .bind(i32::try_from(order).unwrap_or(i32::MAX))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let company: Option<String> = sqlx::query_scalar("SELECT name FROM company WHERE id = ?")
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?
        .flatten();
    let company = company.unwrap_or_else(|| format!("Company {}", user.company_id));
    let name = user.name.clone().unwrap_or_default();
    let email = OutgoingEmail {
        from_name: Some("Granite Manager".into()),
        from_address: NO_REPLY.into(),
        to: vec![NOTIFY.into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: format!("[Bug Report] {title}"),
        body: email_html(
            &title,
            description,
            (&name, &user.email),
            (&company, user.company_id),
            &files,
        ),
        signature: None,
        reply_to: vec![user.email.clone()],
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    let notified = match state.webhooks.send_email(&email).await {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(?err, bug_report = id, "bug report email failed");
            false
        }
    };
    Ok((
        StatusCode::CREATED,
        Json(FiledBugReport {
            id: i32::try_from(id).unwrap_or(i32::MAX),
            notified,
        }),
    ))
}

/// List bug reports
///
/// Every company's reports, newest first, with the reporter, the company
/// and the files. Super admins only.
#[utoipa::path(
    get,
    path = "/v1/bug-reports",
    operation_id = "list_bug_reports",
    tag = "support",
    responses(
        (status = 200, description = "Reports", body = Vec<BugReport>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a super admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Vec<BugReport>>> {
    if !user.is_superuser {
        return Err(ApiError::Forbidden);
    }
    let mut reports: Vec<BugReport> = sqlx::query_as(
        "SELECT br.id, br.user_id, br.company_id, br.title, br.description, br.created_at,
                u.name AS user_name, u.email AS user_email, c.name AS company_name
           FROM bug_reports br
           LEFT JOIN users u ON u.id = br.user_id
           LEFT JOIN company c ON c.id = br.company_id
          ORDER BY br.created_at DESC, br.id DESC",
    )
    .fetch_all(&state.pool)
    .await?;
    let images: Vec<(i32, String)> =
        sqlx::query_as("SELECT bug_report_id, url FROM bug_report_images ORDER BY sort_order, id")
            .fetch_all(&state.pool)
            .await?;
    let mut by_report: HashMap<i32, Vec<String>> = HashMap::new();
    for (report, url) in images {
        by_report.entry(report).or_default().push(url);
    }
    for report in &mut reports {
        report.image_urls = by_report.remove(&report.id).unwrap_or_default();
    }
    Ok(Json(reports))
}

#[cfg(test)]
mod tests {
    use super::title_of;

    #[test]
    fn the_title_is_the_first_line() {
        assert_eq!(title_of("  Save fails\nwhen I click"), "Save fails");
        let long = "x".repeat(250);
        let title = title_of(&long);
        assert_eq!(title.chars().count(), 200);
        assert!(title.ends_with('…'));
    }
}
