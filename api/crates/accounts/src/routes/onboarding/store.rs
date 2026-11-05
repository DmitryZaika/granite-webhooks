//! The onboarding store: the first-run journey, feature tours and points.
//!
//! Kept per user and HOME company. Port of the app's
//! `onboardingStore.server.ts` and `onboardingEvents.server.ts`; their
//! comments explain each rule.

use super::manifest::{self, price_for_row};
use crate::openapi::ToSchema;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::{MySql, MySqlConnection, MySqlPool, QueryBuilder};
use std::collections::{BTreeMap, HashMap};

/// A client clock more than this far ahead of the server is treated as "now".
const FUTURE_TOLERANCE_MINUTES: i64 = 5;

/// Every table an onboarding journey lives in; scoped by (user, company).
const TABLES: [&str; 5] = [
    "user_onboarding",
    "user_onboarding_modules",
    "user_feature_tasks",
    "user_points_ledger",
    "user_feature_task_sightings",
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

/// What started a tour (`task.position`'s `trigger`).
///
/// The app opened it by itself (`auto`), the user asked for it (`user`), or
/// the user's own use of the feature opened it (`gesture`). Kept from the
/// first event that says.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TaskTrigger {
    Auto,
    User,
    Gesture,
}

impl TaskTrigger {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::User => "user",
            Self::Gesture => "gesture",
        }
    }
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
        /// A replay of a tour whose feature the user already uses: the tour
        /// will pay no points. Never applied to a completed tour.
        usage_skipped: Option<bool>,
        /// What started the tour.
        trigger: Option<TaskTrigger>,
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
    /// The user met the tour's opening control on its own page before the
    /// tour ever ran. The first sighting is kept; it creates no tour row.
    #[serde(rename = "task.sighted", rename_all = "camelCase")]
    TaskSighted { at: String, task_id: String },
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
            | Self::TaskCompleted { at, .. }
            | Self::TaskSighted { at, .. } => at,
        }
    }

    /// The task of a task event.
    pub fn task_id(&self) -> Option<&str> {
        match self {
            Self::TaskPosition { task_id, .. }
            | Self::TaskStepDone { task_id, .. }
            | Self::TaskClosed { task_id, .. }
            | Self::TaskCompleted { task_id, .. }
            | Self::TaskSighted { task_id, .. } => Some(task_id),
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
            Self::TaskSighted { task_id, .. } => (!task_ok(task_id)).then_some("Invalid task"),
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
    /// When it was last closed (UTC, ISO 8601): a skip, unless completed since.
    pub closed_at: Option<String>,
    /// When the tour was replayed as a feature the user already uses (UTC,
    /// ISO 8601), or when the server found they had used it well before the
    /// tour's row was created. Such a tour pays 0 points. Null otherwise.
    pub usage_skipped_at: Option<String>,
    /// What started the tour: `auto`, `user` or `gesture` (the first event
    /// that said). Null when no event said.
    pub start_trigger: Option<String>,
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
    /// Sightings of feature tours, by task id: when the user first met the
    /// tour's opening control on the tour's own page (UTC, ISO 8601), before
    /// the tour ever ran. A sighting at or before the user's feature-tour
    /// reset is left out. The app starts such a tour by itself only on a
    /// later local day than its sighting. A sighting is not tour progress:
    /// it adds no entry to `tasks`.
    pub sightings: BTreeMap<String, String>,
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
    /// Features the user already uses: usage check id -> when they first used
    /// it (UTC, ISO 8601). Filled only by `GET /v1/me/onboarding`, and only
    /// while the company's guided tours are on; empty elsewhere.
    pub usage: BTreeMap<String, String>,
    /// When the user's account was created (UTC, ISO 8601). Filled only by
    /// `GET /v1/me/onboarding`; null elsewhere.
    pub account_created_at: Option<String>,
}

pub(super) const ISO: &str = "'%Y-%m-%dT%H:%i:%s.000Z'";

fn later(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a > b { a } else { b }),
        (a, b) => a.or(b),
    }
}

type JourneyRow = (
    i32,
    Option<String>,
    Option<String>,
    bool,
    Option<String>,
    bool,
    Option<String>,
);
type ModuleRow = (i32, String, Option<String>, bool, Option<String>);
type TaskRow = (
    i32,
    String,
    i64,
    String,
    Option<String>,
    Option<Value>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// The record of one user (by user only: a changed company still reads).
pub async fn load_record(pool: &MySqlPool, user_id: i32) -> sqlx::Result<OnboardingRecord> {
    Ok(load_records(pool, &[user_id])
        .await?
        .remove(&user_id)
        .unwrap_or_default())
}

/// `SELECT ... WHERE user_id IN (...) <rest>`, bound to `users`.
fn by_users(select: &str, users: &[i32], rest: &str) -> QueryBuilder<MySql> {
    let mut query = QueryBuilder::new(format!("{select} WHERE user_id IN ("));
    let mut list = query.separated(", ");
    for user in users {
        list.push_bind(*user);
    }
    query.push(") ");
    query.push(rest);
    query
}

/// The records of many users at once (a handful of queries in all); every user asked
/// for has one, empty when they never started.
pub async fn load_records(
    pool: &MySqlPool,
    users: &[i32],
) -> sqlx::Result<HashMap<i32, OnboardingRecord>> {
    let mut records: HashMap<i32, OnboardingRecord> = users
        .iter()
        .map(|user| (*user, OnboardingRecord::default()))
        .collect();
    if users.is_empty() {
        return Ok(records);
    }
    let journeys: Vec<JourneyRow> = by_users(
        &format!(
            "SELECT user_id, current_module_id, current_step_key, dismissed_at IS NOT NULL,
                    DATE_FORMAT(completed_at, {ISO}), feedback_dismissed_at IS NOT NULL,
                    DATE_FORMAT(updated_at, {ISO})
               FROM user_onboarding"
        ),
        users,
        "",
    )
    .build_query_as()
    .fetch_all(pool)
    .await?;
    for (user, module, step, dismissed, completed_at, feedback, updated) in journeys {
        let Some(record) = records.get_mut(&user) else {
            continue;
        };
        record.has_row = true;
        record.completed_at = completed_at;
        let module = module.filter(|id| manifest::is_module(id));
        record.state.current_step_key = module.as_ref().and(step);
        record.state.current_module_id = module;
        record.state.dismissed = dismissed;
        record.state.feedback_dismissed = feedback;
        record.state.updated_at = updated;
    }
    let modules: Vec<ModuleRow> = by_users(
        &format!(
            "SELECT user_id, module_id, step_key, completed_at IS NOT NULL,
                    DATE_FORMAT(updated_at, {ISO})
               FROM user_onboarding_modules"
        ),
        users,
        "ORDER BY module_id",
    )
    .build_query_as()
    .fetch_all(pool)
    .await?;
    for (user, module, step, completed, updated) in modules {
        let Some(record) = records.get_mut(&user) else {
            continue;
        };
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
    fill_tasks(pool, users, &mut records).await?;
    fill_sightings(pool, users, &mut records).await?;
    Ok(records)
}

/// The tour sightings of [`load_records`]: those after the user's feature-tour
/// reset, for tasks that are not retired.
async fn fill_sightings(
    pool: &MySqlPool,
    users: &[i32],
    records: &mut HashMap<i32, OnboardingRecord>,
) -> sqlx::Result<()> {
    let mut query = QueryBuilder::<MySql>::new(format!(
        "SELECT s.user_id, s.task_id, DATE_FORMAT(s.first_seen_at, {ISO})
           FROM user_feature_task_sightings s
           LEFT JOIN user_onboarding uo
             ON uo.user_id = s.user_id AND uo.company_id = s.company_id
          WHERE s.first_seen_at > COALESCE(uo.feature_tasks_reset_at, '1000-01-01')
            AND s.user_id IN ("
    ));
    let mut list = query.separated(", ");
    for user in users {
        list.push_bind(*user);
    }
    query.push(")");
    let sightings: Vec<(i32, String, String)> = query.build_query_as().fetch_all(pool).await?;
    for (user, task_id, seen) in sightings {
        let Some(record) = records.get_mut(&user) else {
            continue;
        };
        if manifest::is_retired(&task_id) {
            continue;
        }
        record.state.updated_at = later(record.state.updated_at.take(), Some(seen.clone()));
        record.state.sightings.insert(task_id, seen);
    }
    Ok(())
}

/// The feature tours of [`load_records`], with the points each paid.
async fn fill_tasks(
    pool: &MySqlPool,
    users: &[i32],
    records: &mut HashMap<i32, OnboardingRecord>,
) -> sqlx::Result<()> {
    let points: Vec<(i32, String, i64)> = by_users(
        "SELECT user_id, task_id, CAST(SUM(points) AS SIGNED) FROM user_points_ledger",
        users,
        "GROUP BY user_id, task_id",
    )
    .build_query_as()
    .fetch_all(pool)
    .await?;
    let points: HashMap<(i32, String), i64> = points
        .into_iter()
        .map(|(user, task, points)| ((user, task), points))
        .collect();
    let tasks: Vec<TaskRow> = by_users(
        &format!(
            "SELECT user_id, task_id, CAST(version AS SIGNED), status, step_key, done_steps,
                    DATE_FORMAT(started_at, {ISO}), DATE_FORMAT(completed_at, {ISO}),
                    DATE_FORMAT(last_active_at, {ISO}), DATE_FORMAT(updated_at, {ISO}),
                    DATE_FORMAT(closed_at, {ISO}), DATE_FORMAT(usage_skipped_at, {ISO}),
                    CAST(start_trigger AS CHAR)
               FROM user_feature_tasks"
        ),
        users,
        "",
    )
    .build_query_as()
    .fetch_all(pool)
    .await?;
    for (
        user,
        task_id,
        version,
        status,
        step_key,
        done,
        started,
        completed,
        active,
        updated,
        closed,
        usage_skipped,
        trigger,
    ) in tasks
    {
        let Some(record) = records.get_mut(&user) else {
            continue;
        };
        let status = match status.as_str() {
            "completed" | "closed" => status,
            _ => "in_progress".to_string(),
        };
        let awarded = if status == "completed" {
            points
                .get(&(user, task_id.clone()))
                .copied()
                .unwrap_or(0)
                .max(0)
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
                closed_at: closed,
                usage_skipped_at: usage_skipped,
                start_trigger: trigger,
            },
        );
        record.state.updated_at = later(record.state.updated_at.take(), updated);
    }
    Ok(())
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

/// A tour used the feature at least this long before its row was created:
/// the user already knew the feature, so the tour pays nothing. Shorter, and
/// the use may be this very tour's own walkthrough.
const USAGE_GRACE_MINUTES: i64 = 10;

/// What a task event writes beyond the row's progress columns.
#[derive(Clone, Copy, Default)]
struct TaskExtra<'a> {
    /// What started the tour (`task.position` only).
    trigger: Option<TaskTrigger>,
    /// The user skipped the tour as already used (`task.position` only).
    usage_skipped: bool,
    /// The tour's usage check, to find out whether the user used the feature
    /// before this event created the tour's row.
    usage_check: Option<&'a str>,
}

/// The actor used the feature of `check_id` well before `at`: a cached
/// positive past their fence, found before `at`, whose use is at least
/// [`USAGE_GRACE_MINUTES`] older than `at`.
async fn used_before(
    conn: &mut MySqlConnection,
    actor: Actor,
    check_id: &str,
    at: NaiveDateTime,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(
           SELECT 1 FROM user_feature_usage f
             LEFT JOIN user_onboarding uo ON uo.user_id = f.user_id AND uo.company_id = f.company_id
            WHERE f.user_id = ? AND f.company_id = ? AND f.check_id = ?
              AND f.detected_at < ? AND f.first_used_at <= ?
              AND f.first_used_at > COALESCE(uo.feature_tasks_reset_at, '1000-01-01'))",
    )
    .bind(actor.user_id)
    .bind(actor.company_id)
    .bind(check_id)
    .bind(at)
    .bind(at - Duration::minutes(USAGE_GRACE_MINUTES))
    .fetch_one(&mut *conn)
    .await
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
    extra: TaskExtra<'_>,
) -> sqlx::Result<()> {
    let (task_id, version, step_key) = task;
    // The user row is locked for the whole write, so this cannot race another
    // event creating the row. (`rows_affected` cannot tell a new row from an
    // update that changed nothing.)
    let existed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_feature_tasks WHERE user_id = ? AND task_id = ?)",
    )
    .bind(actor.user_id)
    .bind(task_id)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO user_feature_tasks
           (user_id, company_id, task_id, version, status, step_key, done_steps,
            started_at, closed_at, last_active_at, usage_skipped_at, start_trigger)
         VALUES (?, ?, ?, ?, ?, ?, CAST(? AS JSON), ?, ?, ?, ?, ?) AS new
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
    .bind(extra.usage_skipped.then_some(at))
    .bind(extra.trigger.map(TaskTrigger::as_str))
    .execute(&mut *conn)
    .await?;
    // Backstop: a tour that starts for a feature the user has used for a
    // while pays nothing, even if the app never asked to skip it. Only on the
    // event that creates the row, and never on a completed one (a paid tour
    // keeps its points).
    if !existed
        && let Some(check_id) = extra.usage_check
        && used_before(conn, actor, check_id, at).await?
    {
        sqlx::query(
            "UPDATE user_feature_tasks SET usage_skipped_at = ?
              WHERE user_id = ? AND task_id = ? AND usage_skipped_at IS NULL
                AND status <> 'completed'",
        )
        .bind(at)
        .bind(actor.user_id)
        .bind(task_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // one arm per event type, as in the app
async fn apply_one(
    conn: &mut MySqlConnection,
    actor: Actor,
    event: &OnboardingEvent,
    at: NaiveDateTime,
    usage_check: Option<&str>,
) -> sqlx::Result<()> {
    let (user, company) = (actor.user_id, actor.company_id);
    let extra = TaskExtra {
        usage_check,
        ..TaskExtra::default()
    };
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
            usage_skipped,
            trigger,
            ..
        } => {
            // status is assigned LAST: ON DUPLICATE KEY UPDATE assigns left to right,
            // and `usage_skipped_at` reads it. The first trigger sticks; a skip
            // stamps once and never on a completed (paid) tour.
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
                 start_trigger = COALESCE(user_feature_tasks.start_trigger, new.start_trigger),
                 usage_skipped_at = IF(user_feature_tasks.status = 'completed',
                                       user_feature_tasks.usage_skipped_at,
                                       COALESCE(user_feature_tasks.usage_skipped_at, new.usage_skipped_at)),
                 status = IF(user_feature_tasks.status = 'completed', 'completed', 'in_progress')",
                TaskExtra {
                    trigger: *trigger,
                    usage_skipped: usage_skipped.unwrap_or(false),
                    ..extra
                },
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
                extra,
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
                extra,
            )
            .await?;
        }
        OnboardingEvent::TaskSighted { task_id, .. } => {
            // The first sighting stays, unless it is from before the user's
            // reset: then it is replaced. `first_seen_at`, which the stale
            // condition reads, is assigned last. No tour row is created: any
            // row there counts as met.
            sqlx::query(
                "INSERT INTO user_feature_task_sightings (user_id, company_id, task_id, first_seen_at)
                 VALUES (?, ?, ?, ?) AS new
                 ON DUPLICATE KEY UPDATE first_seen_at = IF(
                   user_feature_task_sightings.first_seen_at <= COALESCE(
                     (SELECT uo.feature_tasks_reset_at FROM user_onboarding uo
                       WHERE uo.user_id = new.user_id AND uo.company_id = new.company_id),
                     '1000-01-01'),
                   new.first_seen_at, user_feature_task_sightings.first_seen_at)",
            )
            .bind(user)
            .bind(company)
            .bind(task_id)
            .bind(at)
            .execute(&mut *conn)
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
                extra,
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
            let row: Option<(Value, i64, bool)> = sqlx::query_as(
                "SELECT done_steps, CAST(version AS SIGNED), usage_skipped_at IS NOT NULL
                   FROM user_feature_tasks
                  WHERE user_id = ? AND company_id = ? AND task_id = ?",
            )
            .bind(user)
            .bind(company)
            .bind(task_id)
            .fetch_optional(&mut *conn)
            .await?;
            let (done, recorded, skipped) = row.map_or_else(
                || (Map::new(), None, false),
                |(done, version, skipped)| (done_steps(Some(&done)), Some(version), skipped),
            );
            // A tour of a feature the user already used pays nothing.
            let price = price_for_row(task_id, &done, recorded, skipped);
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
/// a retired task and for a tour of a feature the user already used) with a
/// delta row, so the audit trail stays.
async fn reprice(conn: &mut MySqlConnection, actor: Actor, now: NaiveDateTime) -> sqlx::Result<()> {
    let tasks: Vec<(String, Value, i64, bool)> = sqlx::query_as(
        "SELECT task_id, done_steps, CAST(version AS SIGNED), usage_skipped_at IS NOT NULL
           FROM user_feature_tasks
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
    for (task_id, done, version, skipped) in tasks {
        let live = manifest::task(&task_id).is_some();
        if !live && !manifest::is_retired(&task_id) {
            continue;
        }
        let target = if live {
            price_for_row(&task_id, &done_steps(Some(&done)), Some(version), skipped)
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
    apply_events_checked(pool, actor, events, server_checked, manifest_usage_check).await
}

/// The usage check the manifest names for a task.
fn manifest_usage_check(task_id: &str) -> Option<String> {
    manifest::task(task_id).and_then(|task| task.usage_check.clone())
}

/// [`apply_events`] with the task -> usage check lookup given, so a test can
/// give a task a check the manifest does not.
#[doc(hidden)]
pub async fn apply_events_checked(
    pool: &MySqlPool,
    actor: Actor,
    events: &[OnboardingEvent],
    server_checked: bool,
    usage_check_of: fn(&str) -> Option<String>,
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
        let usage_check = task.and_then(usage_check_of);
        apply_one(&mut tx, actor, event, at, usage_check.as_deref()).await?;
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
