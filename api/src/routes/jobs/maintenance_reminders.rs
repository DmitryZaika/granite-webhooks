//! `POST /v1/jobs/maintenance-reminders`: at 9 AM store time, a Telegram
//! digest of the equipment checks due today to each shop worker of the
//! company. Port of `maintenanceReminders.server.ts`.

use super::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{NaiveDate, Timelike, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use serde::Serialize;
use std::collections::BTreeMap;
use utoipa::ToSchema;

const REMINDER_HOUR: u32 = 9;
/// `positions.id` of a shop worker.
const SHOP_WORKER_POSITION: i32 = 4;

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct DigestRun {
    /// Shop workers with checks due today.
    pub processed: usize,
    /// Digests sent.
    pub sent: usize,
    /// Digests Telegram refused (retried next run).
    pub failed: usize,
    /// Not 9 AM store time: nothing was looked at.
    pub skipped: bool,
}

fn page_url() -> String {
    let base = std::env::var("APP_URL").unwrap_or_else(|_| "https://granite-manager.com".into());
    format!("{}/shop/maintenance", base.trim_end_matches('/'))
}

/// The digest text.
pub fn digest(items: &[String]) -> String {
    let mut lines = vec!["🔧 Maintenance due today".to_string(), String::new()];
    lines.extend(items.iter().map(|item| format!("• {item}")));
    lines.push(String::new());
    lines.push(format!("Open: {}", page_url()));
    lines.join("\n")
}

/// Labels of the checks due on `day`, per company, each once.
async fn due_items(
    state: &AppState,
    day: NaiveDate,
) -> Result<BTreeMap<i32, Vec<String>>, sqlx::Error> {
    let rows: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT mi.company_id, mi.name, si.name
           FROM maintenance_conditions mc
           JOIN maintenance_sub_items si ON si.id = mc.maintenance_sub_item_id AND si.deleted_at IS NULL
           JOIN maintenance_items mi ON mi.id = si.maintenance_item_id AND mi.deleted_at IS NULL
          WHERE mc.deleted_at IS NULL AND mc.next_due_date = ?
          ORDER BY mi.company_id, mi.name, si.name",
    )
    .bind(day)
    .fetch_all(&state.pool)
    .await?;
    let mut due: BTreeMap<i32, Vec<String>> = BTreeMap::new();
    for (company_id, parent, sub) in rows {
        let parent = Some(parent.trim())
            .filter(|name| !name.is_empty())
            .unwrap_or("Equipment");
        let sub = Some(sub.trim())
            .filter(|name| !name.is_empty())
            .unwrap_or("Check");
        let label = format!("{parent} · {sub}");
        let items = due.entry(company_id).or_default();
        if !items.contains(&label) {
            items.push(label);
        }
    }
    Ok(due)
}

/// Send today's maintenance digests
///
/// At 9 AM store time, sends each shop worker with Telegram notifications a
/// list of their company's equipment checks due today, once a day.
#[utoipa::path(
    post,
    path = "/v1/jobs/maintenance-reminders",
    operation_id = "run_maintenance_reminders",
    tag = "jobs",
    responses(
        (status = 200, description = "What was sent", body = DigestRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(State(state): State<AppState>, _job: JobCaller) -> ApiResult<Json<DigestRun>> {
    let now = Utc::now().with_timezone(&Indianapolis);
    if now.hour() != REMINDER_HOUR {
        return Ok(Json(DigestRun {
            skipped: true,
            ..DigestRun::default()
        }));
    }
    Ok(Json(send_digests(&state, now.date_naive()).await?))
}

/// Sends the digests of the checks due on `day`.
pub async fn send_digests(state: &AppState, day: NaiveDate) -> Result<DigestRun, sqlx::Error> {
    let due = due_items(state, day).await?;
    let mut run = DigestRun::default();
    if due.is_empty() {
        return Ok(run);
    }
    let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT DISTINCT u.id, up.company_id FROM users u
           JOIN users_positions up ON up.user_id = u.id AND up.company_id = u.company_id
          WHERE u.is_deleted = 0 AND u.notifications_telegram_id IS NOT NULL AND up.position_id = ",
    );
    builder
        .push_bind(SHOP_WORKER_POSITION)
        .push(" AND up.company_id IN (");
    let mut list = builder.separated(", ");
    for company_id in due.keys() {
        list.push_bind(*company_id);
    }
    builder.push(") ORDER BY up.company_id, u.id");
    let workers: Vec<(i32, i32)> = builder.build_query_as().fetch_all(&state.pool).await?;
    for (user_id, company_id) in workers {
        let Some(items) = due.get(&company_id).filter(|items| !items.is_empty()) else {
            continue;
        };
        run.processed += 1;
        // The unique (user, day) row is the claim: a second run sends nothing.
        let claimed = sqlx::query(
            "INSERT IGNORE INTO maintenance_due_digest_sends (user_id, company_id, due_on, sent_at)
             VALUES (?, ?, ?, UTC_TIMESTAMP())",
        )
        .bind(user_id)
        .bind(company_id)
        .bind(day)
        .execute(&state.pool)
        .await?
        .rows_affected()
            > 0;
        if !claimed {
            continue;
        }
        if state
            .webhooks
            .notify_telegram(user_id, &digest(items))
            .await
        {
            run.sent += 1;
        } else {
            sqlx::query(
                "DELETE FROM maintenance_due_digest_sends WHERE user_id = ? AND due_on = ?",
            )
            .bind(user_id)
            .bind(day)
            .execute(&state.pool)
            .await?;
            run.failed += 1;
        }
    }
    Ok(run)
}
