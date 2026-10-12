//! The onboarding store: the first-run journey, feature tours and points.
//!
//! Kept per user and HOME company. Port of the app's
//! `onboardingStore.server.ts` and `onboardingEvents.server.ts`; their
//! comments explain each rule.

use super::manifest::{self, price_for};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::{MySqlConnection, MySqlPool};
use std::collections::{BTreeMap, HashMap};
use utoipa::ToSchema;

/// A client clock more than this far ahead of the server is treated as "now".
const FUTURE_TOLERANCE_MINUTES: i64 = 5;

/// Every table an onboarding journey lives in; scoped by (user, company).
const TABLES: [&str; 4] = [
    "user_onboarding",
    "user_onboarding_modules",
    "user_feature_tasks",
    "user_points_ledger",
];

/// Who progress is stored for: the user and their HOME company
/// (`users.company_id`), never a super admin's switched company.
#[derive(Debug, Clone, Copy)]
pub struct Actor {
    pub user_id: i32,
    pub company_id: i32,
}

/// The home company of a live user.
pub async fn actor(pool: &MySqlPool, user_id: i32) -> sqlx::Result<Option<Actor>> {
    let company: Option<Option<i32>> =
        sqlx::query_scalar("SELECT company_id FROM users WHERE id = ? AND is_deleted = 0")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(company.flatten().map(|company_id| Actor {
        user_id,
        company_id,
    }))
}

/// A step's completion: done by acting, or marked done by hand.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum StepCompletion {
    Action,
    Manual,
}

/// One onboarding event (`OnboardingEvent` of the app). Replaying one is
/// harmless; events dated before the user's last reset are dropped.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum OnboardingEvent {
    #[serde(rename = "journey.position", rename_all = "camelCase")]
    JourneyPosition {
        at: String,
        module_id: Option<String>,
        step_key: Option<String>,
    },
    #[serde(rename = "journey.module_step", rename_all = "camelCase")]
    JourneyModuleStep {
        at: String,
        module_id: String,
        step_key: Option<String>,
    },
    #[serde(rename = "journey.module_completed", rename_all = "camelCase")]
    JourneyModuleCompleted { at: String, module_id: String },
    #[serde(rename = "journey.module_reopened", rename_all = "camelCase")]
    JourneyModuleReopened { at: String, module_id: String },
    #[serde(rename = "journey.dismissed")]
    JourneyDismissed { at: String },
    #[serde(rename = "journey.resumed")]
    JourneyResumed { at: String },
    #[serde(rename = "journey.completed")]
    JourneyCompleted { at: String },
    #[serde(rename = "journey.feedback_dismissed")]
    JourneyFeedbackDismissed { at: String },
    #[serde(rename = "task.position", rename_all = "camelCase")]
    TaskPosition {
        at: String,
        task_id: String,
        version: i64,
        step_key: Option<String>,
    },
    #[serde(rename = "task.step_done", rename_all = "camelCase")]
    TaskStepDone {
        at: String,
        task_id: String,
        version: i64,
        steps: BTreeMap<String, StepCompletion>,
    },
    #[serde(rename = "task.closed", rename_all = "camelCase")]
    TaskClosed {
        at: String,
        task_id: String,
        version: i64,
        step_key: Option<String>,
    },
    #[serde(rename = "task.completed", rename_all = "camelCase")]
    TaskCompleted {
        at: String,
        task_id: String,
        version: i64,
    },
}

/// Most steps one `task.step_done` may carry.
pub const MAX_STEPS_PER_EVENT: usize = 100;

impl OnboardingEvent {
    fn at(&self) -> &str {
        match self {
            Self::JourneyPosition { at, .. }
            | Self::JourneyModuleStep { at, .. }
            | Self::JourneyModuleCompleted { at, .. }
            | Self::JourneyModuleReopened { at, .. }
            | Self::JourneyDismissed { at }
            | Self::JourneyResumed { at }
            | Self::JourneyCompleted { at }
            | Self::JourneyFeedbackDismissed { at }
            | Self::TaskPosition { at, .. }
            | Self::TaskStepDone { at, .. }
            | Self::TaskClosed { at, .. }
            | Self::TaskCompleted { at, .. } => at,
        }
    }

    /// The task of a task event.
    pub fn task_id(&self) -> Option<&str> {
        match self {
            Self::TaskPosition { task_id, .. }
            | Self::TaskStepDone { task_id, .. }
            | Self::TaskClosed { task_id, .. }
            | Self::TaskCompleted { task_id, .. } => Some(task_id),
            _ => None,
        }
    }

    /// Why the event is malformed (the app's zod schema), if it is.
    pub fn problem(&self) -> Option<&'static str> {
        if DateTime::parse_from_rfc3339(self.at()).is_err() {
            return Some("Invalid timestamp");
        }
        let step_ok = |key: &Option<String>| {
            key.as_ref()
                .is_none_or(|key| (1..=128).contains(&key.chars().count()))
        };
        let task_ok = |id: &str| (1..=64).contains(&id.chars().count());
        match self {
            Self::JourneyPosition {
                module_id,
                step_key,
                ..
            } => {
                if module_id
                    .as_deref()
                    .is_some_and(|id| !manifest::is_module(id))
                {
                    return Some("Unknown module");
                }
                (!step_ok(step_key)).then_some("Invalid step key")
            }
            Self::JourneyModuleStep {
                module_id,
                step_key,
                ..
            } => {
                if !manifest::is_module(module_id) {
                    return Some("Unknown module");
                }
                (!step_ok(step_key)).then_some("Invalid step key")
            }
            Self::JourneyModuleCompleted { module_id, .. }
            | Self::JourneyModuleReopened { module_id, .. } => {
                (!manifest::is_module(module_id)).then_some("Unknown module")
            }
            Self::TaskPosition {
                task_id,
                version,
                step_key,
                ..
            }
            | Self::TaskClosed {
                task_id,
                version,
                step_key,
                ..
            } => {
                if !task_ok(task_id) || *version < 0 {
                    return Some("Invalid task");
                }
                (!step_ok(step_key)).then_some("Invalid step key")
            }
            Self::TaskStepDone {
                task_id,
                version,
                steps,
                ..
            } => {
                if !task_ok(task_id) || *version < 0 {
                    return Some("Invalid task");
                }
                if steps.len() > MAX_STEPS_PER_EVENT
                    || steps
                        .keys()
                        .any(|key| !(1..=128).contains(&key.chars().count()))
                {
                    return Some("Invalid steps");
                }
                None
            }
            Self::TaskCompleted {
                task_id, version, ..
            } => (!task_ok(task_id) || *version < 0).then_some("Invalid task"),
            _ => None,
        }
    }
}

/// When the event happened: its own time, unless unreadable or in the future.
fn event_time(at: &str, now: NaiveDateTime) -> NaiveDateTime {
    DateTime::parse_from_rfc3339(at).map_or(now, |time| {
        let time = time.with_timezone(&Utc).naive_utc();
        if time - now > Duration::minutes(FUTURE_TOLERANCE_MINUTES) {
            now
        } else {
            time
        }
    })
}

/// A feature tour's saved progress.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TaskProgress {
    /// Registry version the progress was recorded against.
    pub version: i64,
    /// `in_progress`, `closed` or `completed`.
    pub status: String,
    /// The step the user is on.
    pub step_key: Option<String>,
    /// Steps done, by stored key.
    #[schema(value_type = Object)]
    pub done_steps: Map<String, Value>,
    /// When it was completed (UTC, ISO 8601).
    pub completed_at: Option<String>,
    /// Points it paid (completed tours only, never negative).
    pub points_awarded: i64,
    /// Last activity (UTC, ISO 8601).
    pub last_active_at: Option<String>,
    /// When it started (UTC, ISO 8601).
    pub started_at: Option<String>,
}

/// A user's saved onboarding.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct OnboardingState {
    /// The first-run module the user is in.
    pub current_module_id: Option<String>,
    /// Its step.
    pub current_step_key: Option<String>,
    /// The first-run journey was put away.
    pub dismissed: bool,
    /// The feedback prompt was put away.
    pub feedback_dismissed: bool,
    /// Finished first-run modules, by id.
    pub completed_module_ids: Vec<String>,
    /// Each started module's step key.
    pub module_step_keys: BTreeMap<String, String>,
    /// Feature tours, by task id.
    pub tasks: BTreeMap<String, TaskProgress>,
    /// Points of completed tours.
    pub points: i64,
    /// Latest change (UTC, ISO 8601).
    pub updated_at: Option<String>,
}

/// A user's onboarding record.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct OnboardingRecord {
    /// The saved state.
    pub state: OnboardingState,
    /// When the first-run journey was completed.
    pub completed_at: Option<String>,
    /// A journey row exists (an empty but dated one counts).
    pub has_row: bool,
}

const ISO: &str = "'%Y-%m-%dT%H:%i:%s.000Z'";

fn later(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a > b { a } else { b }),
        (a, b) => a.or(b),
    }
}

type JourneyRow = (
    Option<String>,
    Option<String>,
    bool,
    Option<String>,
    bool,
    Option<String>,
);
type ModuleRow = (String, Option<String>, bool, Option<String>);
type TaskRow = (
    String,
    i64,
    String,
    Option<String>,
    Option<Value>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// The record of one user (by user only: a changed company still reads).
pub async fn load_record(pool: &MySqlPool, user_id: i32) -> sqlx::Result<OnboardingRecord> {
    let mut record = OnboardingRecord::default();
    let journey: Option<JourneyRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT current_module_id, current_step_key, dismissed_at IS NOT NULL,
                DATE_FORMAT(completed_at, {ISO}), feedback_dismissed_at IS NOT NULL,
                DATE_FORMAT(updated_at, {ISO})
           FROM user_onboarding WHERE user_id = ?"
    )))
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    if let Some((module, step, dismissed, completed_at, feedback, updated)) = journey {
        record.has_row = true;
        record.completed_at = completed_at;
        let module = module.filter(|id| manifest::is_module(id));
        record.state.current_step_key = module.as_ref().and(step);
        record.state.current_module_id = module;
        record.state.dismissed = dismissed;
        record.state.feedback_dismissed = feedback;
        record.state.updated_at = updated;
    }
    let modules: Vec<ModuleRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT module_id, step_key, completed_at IS NOT NULL, DATE_FORMAT(updated_at, {ISO})
           FROM user_onboarding_modules WHERE user_id = ? ORDER BY module_id"
    )))
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    for (module, step, completed, updated) in modules {
        if !manifest::is_module(&module) {
            continue;
        }
        if completed {
            record.state.completed_module_ids.push(module.clone());
        }
        if let Some(step) = step {
            record.state.module_step_keys.insert(module, step);
        }
        record.state.updated_at = later(record.state.updated_at.take(), updated);
    }
    let points: Vec<(String, i64)> = sqlx::query_as(
        "SELECT task_id, CAST(SUM(points) AS SIGNED) FROM user_points_ledger
          WHERE user_id = ? GROUP BY task_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let points: HashMap<String, i64> = points.into_iter().collect();
    let tasks: Vec<TaskRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT task_id, CAST(version AS SIGNED), status, step_key, done_steps,
                DATE_FORMAT(started_at, {ISO}), DATE_FORMAT(completed_at, {ISO}),
                DATE_FORMAT(last_active_at, {ISO}), DATE_FORMAT(updated_at, {ISO})
           FROM user_feature_tasks WHERE user_id = ?"
    )))
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    for (task_id, version, status, step_key, done, started, completed, active, updated) in tasks {
        let status = match status.as_str() {
            "completed" | "closed" => status,
            _ => "in_progress".to_string(),
        };
        let awarded = if status == "completed" {
            points.get(&task_id).copied().unwrap_or(0).max(0)
        } else {
            0
        };
        record.state.points += awarded;
        record.state.tasks.insert(
            task_id,
            TaskProgress {
                version,
                status,
                step_key,
                done_steps: done_steps(done.as_ref()),
                completed_at: completed,
                points_awarded: awarded,
                last_active_at: active,
                started_at: started,
            },
        );
        record.state.updated_at = later(record.state.updated_at.take(), updated);
    }
    Ok(record)
}

/// `parseDoneSteps`: only `action` / `manual` entries.
fn done_steps(value: Option<&Value>) -> Map<String, Value> {
    let parsed = match value {
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(Value::Null),
        Some(value) => value.clone(),
        None => Value::Null,
    };
    let Value::Object(map) = parsed else {
        return Map::new();
    };
    map.into_iter()
        .filter(|(_, value)| matches!(value.as_str(), Some("action" | "manual")))
        .collect()
}

/// Locks the user row and checks their home company is still the actor's.
async fn lock_home_company(
    conn: &mut MySqlConnection,
    actor: Actor,
    allow_deleted: bool,
) -> sqlx::Result<bool> {
    let row: Option<(Option<i32>, bool)> = sqlx::query_as(
        "SELECT company_id, COALESCE(is_deleted, 0) <> 0 FROM users WHERE id = ? FOR UPDATE",
    )
    .bind(actor.user_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.is_some_and(|(company, deleted)| {
        (allow_deleted || !deleted) && company == Some(actor.company_id)
    }))
}

/// Re-files rows saved under another company, then makes sure the journey row exists.
async fn prepare(conn: &mut MySqlConnection, actor: Actor) -> sqlx::Result<()> {
    for table in TABLES {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET company_id = ? WHERE user_id = ? AND company_id <> ?"
        )))
        .bind(actor.company_id)
        .bind(actor.user_id)
        .bind(actor.company_id)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query(
        "INSERT INTO user_onboarding (user_id, company_id) VALUES (?, ?)
         ON DUPLICATE KEY UPDATE user_id = user_id",
    )
    .bind(actor.user_id)
    .bind(actor.company_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The reset fences: journey events and task events before them are dropped.
async fn lock_fences(
    conn: &mut MySqlConnection,
    actor: Actor,
) -> sqlx::Result<(Option<NaiveDateTime>, Option<NaiveDateTime>)> {
    let row: Option<(Option<NaiveDateTime>, Option<NaiveDateTime>)> = sqlx::query_as(
        "SELECT reset_at, feature_tasks_reset_at FROM user_onboarding
          WHERE user_id = ? AND company_id = ? FOR UPDATE",
    )
    .bind(actor.user_id)
    .bind(actor.company_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.unwrap_or((None, None)))
}

/// How applying went.
pub enum Applied {
    /// The user is gone or moved company meanwhile.
    UnknownUser,
    /// Applied; the home company's Onboarding switch as read inside the write.
    Done { feature_tasks_enabled: bool },
}

#[allow(clippy::too_many_arguments)] // the row's columns, as the app's upsertTask
async fn upsert_task(
    conn: &mut MySqlConnection,
    actor: Actor,
    task: (&str, i64, Option<&str>),
    status: &str,
    done: &str,
    at: NaiveDateTime,
    closed: bool,
    set: &str,
) -> sqlx::Result<()> {
    let (task_id, version, step_key) = task;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO user_feature_tasks
           (user_id, company_id, task_id, version, status, step_key, done_steps,
            started_at, closed_at, last_active_at)
         VALUES (?, ?, ?, ?, ?, ?, CAST(? AS JSON), ?, ?, ?) AS new
         ON DUPLICATE KEY UPDATE {set}"
    )))
    .bind(actor.user_id)
    .bind(actor.company_id)
    .bind(task_id)
    .bind(version)
    .bind(status)
    .bind(step_key)
    .bind(done)
    .bind(at)
    .bind(closed.then_some(at))
    .bind(at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_lines)] // one arm per event type, as in the app
async fn apply_one(
    conn: &mut MySqlConnection,
    actor: Actor,
    event: &OnboardingEvent,
    at: NaiveDateTime,
) -> sqlx::Result<()> {
    let (user, company) = (actor.user_id, actor.company_id);
    let module_step =
        "INSERT INTO user_onboarding_modules (user_id, company_id, module_id, step_key)
                       VALUES (?, ?, ?, ?) AS new
                       ON DUPLICATE KEY UPDATE step_key = new.step_key";
    match event {
        OnboardingEvent::JourneyPosition {
            module_id,
            step_key,
            ..
        } => {
            sqlx::query(
                "UPDATE user_onboarding SET current_module_id = ?, current_step_key = ?
                  WHERE user_id = ? AND company_id = ?",
            )
            .bind(module_id)
            .bind(module_id.as_ref().and(step_key.as_ref()))
            .bind(user)
            .bind(company)
            .execute(&mut *conn)
            .await?;
            if let (Some(module), Some(step)) = (module_id, step_key) {
                sqlx::query(module_step)
                    .bind(user)
                    .bind(company)
                    .bind(module)
                    .bind(step)
                    .execute(&mut *conn)
                    .await?;
            }
        }
        OnboardingEvent::JourneyModuleStep {
            module_id,
            step_key,
            ..
        } => {
            sqlx::query(module_step)
                .bind(user)
                .bind(company)
                .bind(module_id)
                .bind(step_key)
                .execute(&mut *conn)
                .await?;
        }
        OnboardingEvent::JourneyModuleCompleted { module_id, .. } => {
            sqlx::query(
                "INSERT INTO user_onboarding_modules (user_id, company_id, module_id, step_key, completed_at)
                 VALUES (?, ?, ?, NULL, ?) AS new
                 ON DUPLICATE KEY UPDATE
                   completed_at = COALESCE(user_onboarding_modules.completed_at, new.completed_at),
                   step_key = NULL",
            )
            .bind(user)
            .bind(company)
            .bind(module_id)
            .bind(at)
            .execute(&mut *conn)
            .await?;
            sqlx::query(
                "UPDATE user_onboarding SET current_module_id = NULL, current_step_key = NULL
                  WHERE user_id = ? AND company_id = ? AND current_module_id = ?",
            )
            .bind(user)
            .bind(company)
            .bind(module_id)
            .execute(&mut *conn)
            .await?;
        }
        OnboardingEvent::JourneyModuleReopened { module_id, .. } => {
            sqlx::query(
                "UPDATE user_onboarding_modules SET completed_at = NULL
                  WHERE user_id = ? AND company_id = ? AND module_id = ?",
            )
            .bind(user)
            .bind(company)
            .bind(module_id)
            .execute(&mut *conn)
            .await?;
        }
        OnboardingEvent::JourneyDismissed { .. } => {
            sqlx::query(
                "UPDATE user_onboarding SET dismissed_at = ? WHERE user_id = ? AND company_id = ?",
            )
            .bind(at)
            .bind(user)
            .bind(company)
            .execute(&mut *conn)
            .await?;
        }
        OnboardingEvent::JourneyResumed { .. } => {
            sqlx::query("UPDATE user_onboarding SET dismissed_at = NULL WHERE user_id = ? AND company_id = ?")
                .bind(user)
                .bind(company)
                .execute(&mut *conn)
                .await?;
        }
        OnboardingEvent::JourneyCompleted { .. } => {
            sqlx::query(
                "UPDATE user_onboarding
                    SET completed_at = COALESCE(completed_at, ?), current_module_id = NULL,
                        current_step_key = NULL, dismissed_at = NULL
                  WHERE user_id = ? AND company_id = ?",
            )
            .bind(at)
            .bind(user)
            .bind(company)
            .execute(&mut *conn)
            .await?;
        }
        OnboardingEvent::JourneyFeedbackDismissed { .. } => {
            sqlx::query(
                "UPDATE user_onboarding SET feedback_dismissed_at = COALESCE(feedback_dismissed_at, ?)
                  WHERE user_id = ? AND company_id = ?",
            )
            .bind(at)
            .bind(user)
            .bind(company)
            .execute(&mut *conn)
            .await?;
        }
        OnboardingEvent::TaskPosition {
            task_id,
            version,
            step_key,
            ..
        } => {
            // status is assigned LAST: ON DUPLICATE KEY UPDATE assigns left to right.
            upsert_task(
                conn,
                actor,
                (task_id, *version, step_key.as_deref()),
                "in_progress",
                "{}",
                at,
                false,
                "version = new.version, step_key = new.step_key,
                 last_active_at = new.last_active_at,
                 status = IF(user_feature_tasks.status = 'completed', 'completed', 'in_progress')",
            )
            .await?;
        }
        OnboardingEvent::TaskStepDone {
            task_id,
            version,
            steps,
            ..
        } => {
            let done = serde_json::to_string(steps).unwrap_or_else(|_| "{}".into());
            upsert_task(
                conn,
                actor,
                (task_id, *version, None),
                "in_progress",
                &done,
                at,
                false,
                "version = new.version,
                 done_steps = JSON_MERGE_PATCH(user_feature_tasks.done_steps, new.done_steps),
                 last_active_at = new.last_active_at",
            )
            .await?;
        }
        OnboardingEvent::TaskClosed {
            task_id,
            version,
            step_key,
            ..
        } => {
            // A close of a completed task leaves it as it is.
            upsert_task(
                conn,
                actor,
                (task_id, *version, step_key.as_deref()),
                "closed",
                "{}",
                at,
                true,
                "version = new.version,
                 closed_at = IF(user_feature_tasks.status = 'completed', user_feature_tasks.closed_at, new.closed_at),
                 step_key = IF(user_feature_tasks.status = 'completed', user_feature_tasks.step_key, new.step_key),
                 last_active_at = IF(user_feature_tasks.status = 'completed', user_feature_tasks.last_active_at, new.last_active_at),
                 status = IF(user_feature_tasks.status = 'completed', 'completed', 'closed')",
            )
            .await?;
        }
        OnboardingEvent::TaskCompleted {
            task_id, version, ..
        } => {
            upsert_task(
                conn,
                actor,
                (task_id, *version, None),
                "in_progress",
                "{}",
                at,
                false,
                "version = new.version",
            )
            .await?;
            let changed = sqlx::query(
                "UPDATE user_feature_tasks
                    SET status = 'completed', completed_at = ?, step_key = NULL, last_active_at = ?
                  WHERE user_id = ? AND company_id = ? AND task_id = ? AND status <> 'completed'",
            )
            .bind(at)
            .bind(at)
            .bind(user)
            .bind(company)
            .bind(task_id)
            .execute(&mut *conn)
            .await?
            .rows_affected();
            if changed != 1 {
                return Ok(());
            }
            let row: Option<(Value, i64)> = sqlx::query_as(
                "SELECT done_steps, CAST(version AS SIGNED) FROM user_feature_tasks
                  WHERE user_id = ? AND company_id = ? AND task_id = ?",
            )
            .bind(user)
            .bind(company)
            .bind(task_id)
            .fetch_optional(&mut *conn)
            .await?;
            let (done, recorded) = row.map_or_else(
                || (Map::new(), None),
                |(done, version)| (done_steps(Some(&done)), Some(version)),
            );
            let price = price_for(task_id, &done, recorded);
            if price > 0 {
                sqlx::query(
                    "INSERT INTO user_points_ledger (user_id, company_id, task_id, points, reason, awarded_at)
                     VALUES (?, ?, ?, ?, 'completed', ?)",
                )
                .bind(user)
                .bind(company)
                .bind(task_id)
                .bind(price)
                .bind(at)
                .execute(&mut *conn)
                .await?;
            }
        }
    }
    Ok(())
}

/// Keeps each completed task's ledger sum equal to its registry price (0 for
/// a retired task) with a delta row, so the audit trail stays.
async fn reprice(conn: &mut MySqlConnection, actor: Actor, now: NaiveDateTime) -> sqlx::Result<()> {
    let tasks: Vec<(String, Value, i64)> = sqlx::query_as(
        "SELECT task_id, done_steps, CAST(version AS SIGNED) FROM user_feature_tasks
          WHERE user_id = ? AND company_id = ? AND status = 'completed'",
    )
    .bind(actor.user_id)
    .bind(actor.company_id)
    .fetch_all(&mut *conn)
    .await?;
    let sums: Vec<(String, i64)> = sqlx::query_as(
        "SELECT task_id, CAST(SUM(points) AS SIGNED) FROM user_points_ledger
          WHERE user_id = ? AND company_id = ? GROUP BY task_id",
    )
    .bind(actor.user_id)
    .bind(actor.company_id)
    .fetch_all(&mut *conn)
    .await?;
    let ledger: HashMap<String, i64> = sums.into_iter().collect();
    for (task_id, done, version) in tasks {
        let live = manifest::task(&task_id).is_some();
        if !live && !manifest::is_retired(&task_id) {
            continue;
        }
        let target = if live {
            price_for(&task_id, &done_steps(Some(&done)), Some(version))
        } else {
            0
        };
        let delta = target - ledger.get(&task_id).copied().unwrap_or(0);
        if delta == 0 {
            continue;
        }
        sqlx::query(
            "INSERT INTO user_points_ledger (user_id, company_id, task_id, points, reason, awarded_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(actor.user_id)
        .bind(actor.company_id)
        .bind(&task_id)
        .bind(delta)
        .bind(if live { "repriced" } else { "retired" })
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Applies events in one transaction.
///
/// Task events are dropped while the home company's Onboarding switch is
/// off, for retired tasks, and for checklist tasks unless `server_checked`
/// (only the server's own check completes those).
pub async fn apply_events(
    pool: &MySqlPool,
    actor: Actor,
    events: &[OnboardingEvent],
    server_checked: bool,
) -> sqlx::Result<Applied> {
    let tasks_enabled = crate::routes::company::queries::company_features(pool, actor.company_id)
        .await?
        .onboarding;
    let now = Utc::now().naive_utc();
    let mut tx = pool.begin().await?;
    if !lock_home_company(&mut tx, actor, false).await? {
        return Ok(Applied::UnknownUser);
    }
    prepare(&mut tx, actor).await?;
    let (journey_fence, task_fence) = lock_fences(&mut tx, actor).await?;
    for event in events {
        let task = event.task_id();
        if let Some(task_id) = task {
            if !tasks_enabled || manifest::is_retired(task_id) {
                continue;
            }
            if !server_checked && manifest::task(task_id).is_some_and(|task| task.checklist) {
                continue;
            }
        }
        let at = event_time(event.at(), now);
        let fence = if task.is_some() {
            task_fence
        } else {
            journey_fence
        };
        if fence.is_some_and(|fence| at < fence) {
            continue;
        }
        apply_one(&mut tx, actor, event, at).await?;
    }
    if tasks_enabled {
        reprice(&mut tx, actor, now).await?;
    }
    tx.commit().await?;
    Ok(Applied::Done {
        feature_tasks_enabled: tasks_enabled,
    })
}

/// The Hub's Restart: resets the first-run journey; feature tours and points stay.
pub async fn reset_journey(pool: &MySqlPool, actor: Actor) -> sqlx::Result<bool> {
    let now = Utc::now().naive_utc();
    let mut tx = pool.begin().await?;
    if !lock_home_company(&mut tx, actor, false).await? {
        return Ok(false);
    }
    prepare(&mut tx, actor).await?;
    lock_fences(&mut tx, actor).await?;
    sqlx::query("DELETE FROM user_onboarding_modules WHERE user_id = ? AND company_id = ?")
        .bind(actor.user_id)
        .bind(actor.company_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE user_onboarding
            SET reset_at = ?, current_module_id = NULL, current_step_key = NULL,
                dismissed_at = NULL, completed_at = NULL
          WHERE user_id = ? AND company_id = ?",
    )
    .bind(now)
    .bind(actor.user_id)
    .bind(actor.company_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}
