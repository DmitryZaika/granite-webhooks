//! `POST /v1/jobs/survey-notifications`: the post-installation survey.
//!
//! A day after an installer's checklist is signed the customer gets the
//! survey link by email and text; reminders follow on days 3 and 7 (a text
//! from the office manager's `CloudTalk` line, else email), plus an email on
//! day 5 for customers reminded by text. Only in the evening on weekdays or
//! midday on weekends. Port of `checklistSurveyNotify.server.ts`.
//!
//! Each send is claimed first with a marker row in `emails`
//! (`checklist-survey-<id>`, `checklist-survey-reminder-<n>-<id>`); a
//! submitted survey leaves `checklist-survey-done-<id>`.

use super::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::routes::company::queries::company_features;
use crate::routes::events::notify::{customer_phone, office_manager};
use crate::routes::sms::provider::{SmsProvider, phone10};
use crate::routes::stats::schemas::SmsProvider as CompanyProvider;
use crate::state::AppState;
use crate::webhooks_client::{OutgoingEmail, OutgoingSms};
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Datelike, Duration, Timelike, Utc, Weekday};
use hmac::{Hmac, KeyInit, Mac};
use lambda_http::tracing;
use serde::Serialize;
use sha2::Sha256;
use std::fmt::Write;
use utoipa::ToSchema;

const SENT_PREFIX: &str = "checklist-survey-";
const REMINDER_PREFIX: &str = "checklist-survey-reminder-";
const DONE_PREFIX: &str = "checklist-survey-done-";
const NO_REPLY: &str = "noreply@granite-manager.com";

/// A reminder: its number, the day it is due and how many days it may go out.
#[derive(Debug, Clone, Copy)]
pub struct Reminder {
    pub number: u8,
    pub after_days: i64,
    pub window_days: i64,
    /// Only an email, and only to customers who are reminded by text.
    pub email_only_when_texted: bool,
}

/// In the order they run.
pub const REMINDERS: [Reminder; 3] = [
    Reminder {
        number: 1,
        after_days: 3,
        window_days: 2,
        email_only_when_texted: false,
    },
    Reminder {
        number: 3,
        after_days: 5,
        window_days: 2,
        email_only_when_texted: true,
    },
    Reminder {
        number: 2,
        after_days: 7,
        window_days: 2,
        email_only_when_texted: false,
    },
];

/// One stage's counts.
#[derive(Debug, Default, Clone, Copy, Serialize, ToSchema)]
pub struct StageRun {
    /// Checklists looked at.
    pub processed: usize,
    /// Surveys sent.
    pub sent: usize,
    /// Not due, already sent, or nothing to send to.
    pub skipped: usize,
    /// Failed (an email failure is retried next run).
    pub failed: usize,
}

impl StageRun {
    const fn add(&mut self, other: Self) {
        self.processed += other.processed;
        self.sent += other.sent;
        self.skipped += other.skipped;
        self.failed += other.failed;
    }
}

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct SurveyRun {
    /// Checklists looked at, all stages.
    pub processed: usize,
    /// Surveys sent, all stages.
    pub sent: usize,
    /// Skipped, all stages.
    pub skipped: usize,
    /// Failed, all stages.
    pub failed: usize,
    /// Outside the send window: nothing was looked at.
    pub waiting_for_send_window: bool,
    /// The first invitation.
    pub initial: StageRun,
    /// Day 3 reminder.
    pub reminder1: StageRun,
    /// Day 5 email.
    pub day5_email: StageRun,
    /// Day 7 reminder.
    pub reminder2: StageRun,
}

/// The CRM's `checklistSurveyToken`: 32 hex characters of
/// HMAC-SHA256(`SESSION_SECRET`, `checklist-survey:<id>`).
pub fn survey_token(secret: &str, checklist_id: i32) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("checklist-survey:{checklist_id}").as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in mac.finalize().into_bytes() {
        let _ = write!(hex, "{byte:02x}");
    }
    hex.truncate(32);
    hex
}

fn origin() -> String {
    if let Some(url) = std::env::var("APP_URL")
        .ok()
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
    {
        return url.trim_end_matches('/').to_string();
    }
    match std::env::var("APP_BASE_DOMAIN")
        .ok()
        .map(|domain| domain.trim().to_string())
    {
        Some(domain) if domain.starts_with("http://") || domain.starts_with("https://") => {
            domain.trim_end_matches('/').to_string()
        }
        Some(domain) if !domain.is_empty() => format!("https://{domain}"),
        _ => "https://granite-manager.com".into(),
    }
}

/// The survey page for the checklist, signed, with the checklist's installer
/// (`i`) so the form picks them for the customer.
pub fn survey_url(
    origin: &str,
    secret: &str,
    company_id: i32,
    checklist_id: i32,
    installer_id: Option<i32>,
) -> String {
    let installer = installer_id.map_or_else(String::new, |id| format!("&i={id}"));
    format!(
        "{origin}/customer/{company_id}/survey?c={checklist_id}&t={}{installer}",
        survey_token(secret, checklist_id)
    )
}

/// Weekday evenings 5:00–8:30 PM, weekends 10 AM–5 PM, in
/// `SURVEY_SEND_TIMEZONE` (default New York). `SURVEY_SEND_ANYTIME=1` turns
/// the wait off.
pub fn in_send_window(now: DateTime<Utc>) -> bool {
    if std::env::var("SURVEY_SEND_ANYTIME").is_ok_and(|value| value == "1") {
        return true;
    }
    let zone: chrono_tz::Tz = std::env::var("SURVEY_SEND_TIMEZONE")
        .ok()
        .and_then(|zone| zone.trim().parse().ok())
        .unwrap_or(chrono_tz::America::New_York);
    let local = now.with_timezone(&zone);
    let minute = local.hour() * 60 + local.minute();
    let (from, to) = if matches!(local.weekday(), Weekday::Sat | Weekday::Sun) {
        (10 * 60, 17 * 60)
    } else {
        (17 * 60, 20 * 60 + 30)
    };
    (from..to).contains(&minute)
}

fn message_text(company: &str, url: &str, reminder: bool) -> String {
    if reminder {
        format!(
            "Friendly reminder from {company}: we'd love your feedback on your recent installation. It takes about a minute (just tap the stars) and is fully anonymous: {url}"
        )
    } else {
        format!(
            "Thank you for choosing {company}! How did we do? It takes about a minute (just tap the stars) and is fully anonymous: {url}"
        )
    }
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn email_html(company: &str, url: &str, reminder: bool) -> String {
    let (company, url) = (escape(company), escape(url));
    let intro = if reminder {
        format!(
            r#"<p style="margin:0 0 12px">Friendly reminder from {company}!</p><p style="margin:0 0 20px">We'd love your feedback on your recent installation.</p>"#
        )
    } else {
        format!(
            r#"<p style="margin:0 0 12px">Thank you for choosing {company}!</p><p style="margin:0 0 20px">How did we do? Just tap a few stars.</p>"#
        )
    };
    format!(
        r#"<div style="font-family:Arial,Helvetica,sans-serif;font-size:16px;line-height:1.5;color:#18181b">{intro}<p style="margin:0 0 20px"><a href="{url}" style="display:inline-block;background:#0f172a;color:#ffffff;font-weight:600;text-decoration:none;padding:14px 28px;border-radius:8px">Rate your experience</a></p><p style="margin:0 0 8px;color:#52525b">It takes about a minute and is fully anonymous.</p><p style="margin:0;font-size:13px;color:#71717a">Button not working? Open this link: <a href="{url}">{url}</a></p></div>"#
    )
}

#[derive(sqlx::FromRow)]
struct DueChecklist {
    id: i32,
    company_id: i32,
    customer_id: Option<i32>,
    installer_id: Option<i32>,
    email: Option<String>,
    created_at: DateTime<Utc>,
}

struct Sms {
    provider: SmsProvider,
    manager_id: i32,
    agent_id: String,
    phone_digits: String,
}

async fn sms_target(
    state: &AppState,
    company_id: i32,
    customer_id: Option<i32>,
) -> Result<Option<Sms>, sqlx::Error> {
    let Some(customer_id) = customer_id else {
        return Ok(None);
    };
    let Some(provider) = provider(state, company_id).await? else {
        return Ok(None);
    };
    let Some(manager) = office_manager(&state.pool, company_id, provider).await? else {
        return Ok(None);
    };
    let digits = customer_phone(&state.pool, company_id, customer_id)
        .await?
        .map(|phone| phone10(&phone))
        .filter(|digits| digits.len() == 10);
    Ok(digits.map(|phone_digits| Sms {
        provider,
        manager_id: manager.id,
        agent_id: manager.agent_id,
        phone_digits,
    }))
}

async fn provider(state: &AppState, company_id: i32) -> Result<Option<SmsProvider>, sqlx::Error> {
    Ok(company_features(&state.pool, company_id)
        .await?
        .sms_provider()
        .map(|provider| match provider {
            CompanyProvider::Cloudtalk => SmsProvider::Cloudtalk,
            CompanyProvider::Ringcentral => SmsProvider::Ringcentral,
        }))
}

/// Texts the survey; false when there is no line to text from or it failed.
async fn send_sms(
    state: &AppState,
    company_id: i32,
    customer_id: Option<i32>,
    text: &str,
) -> Result<bool, sqlx::Error> {
    let Some(target) = sms_target(state, company_id, customer_id).await? else {
        return Ok(false);
    };
    let sms = OutgoingSms {
        provider: target.provider.as_str().to_string(),
        company_id,
        user_id: target.manager_id,
        agent_id: target.agent_id,
        phone_digits: target.phone_digits,
        text: text.to_string(),
        attachments: Vec::new(),
        is_automated: true,
        idempotency_key: None,
    };
    match state.webhooks.send_sms(&sms).await {
        Ok(_) => Ok(true),
        Err(failure) => {
            tracing::warn!(error = %failure.error, "checklist survey SMS failed");
            Ok(false)
        }
    }
}

struct EmailFailed;

async fn send_email(
    state: &AppState,
    to: &str,
    company: &str,
    url: &str,
    reminder: bool,
) -> Result<(), EmailFailed> {
    if to.is_empty() {
        return Ok(());
    }
    let email = OutgoingEmail {
        from_name: None,
        from_address: NO_REPLY.into(),
        to: vec![to.to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: if reminder {
            format!("{company} - Reminder: Post-Installation Survey")
        } else {
            format!("{company} - Post-Installation Survey")
        },
        body: email_html(company, url, reminder),
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    state
        .webhooks
        .send_email(&email)
        .await
        .map(|_| ())
        .map_err(|err| {
            tracing::warn!(error = ?err, "checklist survey email failed");
            EmailFailed
        })
}

enum Outcome {
    Sent,
    Skipped,
}

/// The CRM's `sendPostInstallSurveyNotifications`.
async fn notify(
    state: &AppState,
    row: &DueChecklist,
    reminder: Option<Reminder>,
) -> Result<Result<Outcome, EmailFailed>, sqlx::Error> {
    let company: Option<String> = sqlx::query_scalar("SELECT name FROM company WHERE id = ?")
        .bind(row.company_id)
        .fetch_optional(&state.pool)
        .await?;
    let company = company.unwrap_or_else(|| "Our Company".into());
    let url = survey_url(
        &origin(),
        &state.session_secret,
        row.company_id,
        row.id,
        row.installer_id,
    );
    let email = row.email.as_deref().map_or("", str::trim);
    let is_reminder = reminder.is_some();
    let text = message_text(&company, &url, is_reminder);
    if reminder.is_some_and(|config| config.email_only_when_texted) {
        let texted = matches!(
            sms_target(state, row.company_id, row.customer_id).await?,
            Some(Sms {
                provider: SmsProvider::Cloudtalk,
                ..
            })
        );
        if email.is_empty() || !texted {
            return Ok(Ok(Outcome::Skipped));
        }
        return Ok(send_email(state, email, &company, &url, true)
            .await
            .map(|()| Outcome::Sent));
    }
    if is_reminder && provider(state, row.company_id).await? == Some(SmsProvider::Cloudtalk) {
        if send_sms(state, row.company_id, row.customer_id, &text).await? {
            return Ok(Ok(Outcome::Sent));
        }
        return Ok(send_email(state, email, &company, &url, true)
            .await
            .map(|()| Outcome::Sent));
    }
    if let Err(failed) = send_email(state, email, &company, &url, is_reminder).await {
        return Ok(Err(failed));
    }
    send_sms(state, row.company_id, row.customer_id, &text).await?;
    Ok(Ok(Outcome::Sent))
}

async fn marker_exists(state: &AppState, message_id: &str) -> Result<bool, sqlx::Error> {
    let found: Option<i32> =
        sqlx::query_scalar("SELECT id FROM emails WHERE message_id = ? LIMIT 1")
            .bind(message_id)
            .fetch_optional(&state.pool)
            .await?;
    Ok(found.is_some())
}

async fn stage(
    state: &AppState,
    rows: Vec<DueChecklist>,
    now: DateTime<Utc>,
    reminder: Option<Reminder>,
) -> Result<StageRun, sqlx::Error> {
    let mut run = StageRun {
        processed: rows.len(),
        ..StageRun::default()
    };
    let (after, window) = reminder.map_or((1, 2), |config| (config.after_days, config.window_days));
    for row in rows {
        let age = now - row.created_at;
        if age < Duration::days(after) || age > Duration::days(after + window) {
            run.skipped += 1;
            continue;
        }
        let marker = reminder.map_or_else(
            || format!("{SENT_PREFIX}{}", row.id),
            |config| format!("{REMINDER_PREFIX}{}-{}", config.number, row.id),
        );
        if marker_exists(state, &marker).await?
            || (reminder.is_some()
                && marker_exists(state, &format!("{DONE_PREFIX}{}", row.id)).await?)
        {
            run.skipped += 1;
            continue;
        }
        // Claim before sending, so an error after the email went out never
        // sends it again on the next run.
        let subject = reminder.map_or_else(
            || "Post-Installation Survey".to_string(),
            |config| format!("Post-Installation Survey Reminder {}", config.number),
        );
        sqlx::query(
            "INSERT INTO emails (subject, body, message_id, company_id, sender_email, receiver_email, sent_at)
             VALUES (?, '', ?, ?, ?, ?, UTC_TIMESTAMP())",
        )
        .bind(&subject)
        .bind(&marker)
        .bind(row.company_id)
        .bind(NO_REPLY)
        .bind(row.email.as_deref().map_or("", str::trim))
        .execute(&state.pool)
        .await?;
        match notify(state, &row, reminder).await? {
            Ok(Outcome::Sent) => run.sent += 1,
            Ok(Outcome::Skipped) => run.skipped += 1,
            Err(EmailFailed) => {
                // Nothing reached the customer: release the claim to retry.
                sqlx::query("DELETE FROM emails WHERE message_id = ? AND company_id = ?")
                    .bind(&marker)
                    .bind(row.company_id)
                    .execute(&state.pool)
                    .await?;
                run.failed += 1;
            }
        }
    }
    Ok(run)
}

const COLUMNS: &str = "CAST(c.id AS SIGNED) AS id, c.company_id, c.customer_id, c.installer_id, c.email, c.created_at";

/// Every survey stage due at `now` (not checking the send window).
pub async fn send_due(state: &AppState, now: DateTime<Utc>) -> Result<SurveyRun, sqlx::Error> {
    let initial_rows: Vec<DueChecklist> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM checklists c
          WHERE c.created_at <= DATE_SUB(UTC_TIMESTAMP(), INTERVAL 1 DAY)
            AND c.created_at > DATE_SUB(UTC_TIMESTAMP(), INTERVAL 3 DAY)
            AND c.company_id IS NOT NULL
          ORDER BY c.created_at, c.id LIMIT 100"
    )))
    .fetch_all(&state.pool)
    .await?;
    let mut run = SurveyRun {
        initial: stage(state, initial_rows, now, None).await?,
        ..SurveyRun::default()
    };
    for config in REMINDERS {
        let rows: Vec<DueChecklist> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM checklists c
               JOIN emails initial ON initial.message_id = CONCAT('{SENT_PREFIX}', c.id)
              WHERE c.created_at <= DATE_SUB(UTC_TIMESTAMP(), INTERVAL ? DAY)
                AND c.created_at > DATE_SUB(UTC_TIMESTAMP(), INTERVAL ? DAY)
                AND c.company_id IS NOT NULL
                AND initial.sent_at <= DATE_SUB(UTC_TIMESTAMP(), INTERVAL 1 DAY)
                AND NOT EXISTS (SELECT 1 FROM emails r WHERE r.message_id = CONCAT('{REMINDER_PREFIX}', ?, '-', c.id))
                AND NOT EXISTS (SELECT 1 FROM emails d WHERE d.message_id = CONCAT('{DONE_PREFIX}', c.id))
              ORDER BY c.created_at, c.id LIMIT 100"
        )))
        .bind(config.after_days)
        .bind(config.after_days + config.window_days)
        .bind(config.number)
        .fetch_all(&state.pool)
        .await?;
        let result = stage(state, rows, now, Some(config)).await?;
        match config.number {
            1 => run.reminder1 = result,
            3 => run.day5_email = result,
            _ => run.reminder2 = result,
        }
    }
    let mut total = StageRun::default();
    for part in [run.initial, run.reminder1, run.day5_email, run.reminder2] {
        total.add(part);
    }
    run.processed = total.processed;
    run.sent = total.sent;
    run.skipped = total.skipped;
    run.failed = total.failed;
    Ok(run)
}

/// Send due post-installation surveys
///
/// Inside the send window, sends the survey invitations and reminders that
/// are due.
#[utoipa::path(
    post,
    path = "/v1/jobs/survey-notifications",
    operation_id = "run_survey_notifications",
    tag = "jobs",
    responses(
        (status = 200, description = "What was sent", body = SurveyRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(State(state): State<AppState>, _job: JobCaller) -> ApiResult<Json<SurveyRun>> {
    let now = Utc::now();
    if !in_send_window(now) {
        return Ok(Json(SurveyRun {
            waiting_for_send_window: true,
            ..SurveyRun::default()
        }));
    }
    Ok(Json(send_due(&state, now).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_matches_the_crm() {
        // createHmac('sha256', 'secret').update('checklist-survey:42').digest('hex').slice(0, 32)
        assert_eq!(
            survey_token("secret", 42),
            "68213e9577b1507d2d1fa2b7a991204b"
        );
    }

    #[test]
    fn the_window_is_evenings_and_weekend_middays() {
        let at = |text: &str| text.parse::<DateTime<Utc>>().unwrap();
        // Tuesday 6 PM New York (EDT) = 22:00 UTC.
        assert!(in_send_window(at("2026-06-02T22:00:00Z")));
        assert!(!in_send_window(at("2026-06-02T15:00:00Z")));
        // Saturday noon New York.
        assert!(in_send_window(at("2026-06-06T16:00:00Z")));
        assert!(!in_send_window(at("2026-06-06T23:00:00Z")));
    }
}
