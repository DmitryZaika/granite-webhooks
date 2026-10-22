//! `posthog_autofix::intake::tick` against fake PostHog, GitHub and mail.

use api::posthog_autofix::config::{Config, Mode};
use api::posthog_autofix::github::{Dispatch, DispatchError, Dispatcher};
use api::posthog_autofix::intake::{Deps, TickReport, tick};
use api::posthog_autofix::notify::{Email, Mailer};
use api::posthog_autofix::posthog::{Issue, IssueEvents, IssueSource, SourceError};
use api::posthog_autofix::writeback::IssueWriter;
use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};
use serde_json::json;
use sqlx::MySqlPool;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The cutover, aligned to every poll and storm window.
fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 21, 0, 0, 0).unwrap()
}

fn minutes(minutes: i64) -> DateTime<Utc> {
    t0() + Duration::minutes(minutes)
}

fn issue_id(n: u32) -> String {
    format!("01a158ec-0000-4000-8000-{n:012x}")
}

#[derive(Default)]
struct FakeSource {
    issues: Mutex<Vec<Issue>>,
    events: Mutex<Vec<IssueEvents>>,
    calls: AtomicUsize,
}

impl FakeSource {
    fn add(&self, (issue, events): (Issue, IssueEvents)) {
        self.issues.lock().unwrap().push(issue);
        self.events.lock().unwrap().push(events);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl IssueSource for FakeSource {
    async fn issues_since(
        &self,
        since: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<Issue>, SourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut found: Vec<Issue> = self
            .issues
            .lock()
            .unwrap()
            .iter()
            .filter(|issue| issue.created_at >= since)
            .cloned()
            .collect();
        found.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        found.truncate(limit as usize);
        Ok(found)
    }

    async fn issues(&self, ids: &[String]) -> Result<Vec<Issue>, SourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let issues = self.issues.lock().unwrap();
        Ok(issues
            .iter()
            .filter(|issue| ids.contains(&issue.id))
            .cloned()
            .collect())
    }

    async fn events(
        &self,
        ids: &[String],
        _since: DateTime<Utc>,
    ) -> Result<Vec<IssueEvents>, SourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events = self.events.lock().unwrap();
        Ok(events
            .iter()
            .filter(|events| ids.contains(&events.issue_id))
            .cloned()
            .collect())
    }
}

#[derive(Default)]
struct FakeDispatcher {
    /// Run ids of every dispatch attempt.
    attempts: Mutex<Vec<u64>>,
    fail: AtomicBool,
}

impl FakeDispatcher {
    fn attempts(&self) -> Vec<u64> {
        self.attempts.lock().unwrap().clone()
    }
}

impl Dispatcher for FakeDispatcher {
    async fn dispatch(&self, dispatch: &Dispatch<'_>) -> Result<(), DispatchError> {
        self.attempts.lock().unwrap().push(dispatch.run_id);
        if self.fail.load(Ordering::SeqCst) {
            return Err(DispatchError("GitHub dispatch failed: 500 boom".into()));
        }
        Ok(())
    }
}

#[derive(Default)]
struct FakeMailer {
    sent: Mutex<Vec<Email>>,
}

impl FakeMailer {
    fn subjects(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|email| email.subject.clone())
            .collect()
    }
}

impl Mailer for FakeMailer {
    async fn send(&self, _to: &[String], email: &Email) -> Result<(), String> {
        self.sent.lock().unwrap().push(email.clone());
        Ok(())
    }
}

/// Write-back off unless `enabled`; records the issues it assigned.
#[derive(Default)]
struct FakeWriter {
    enabled: AtomicBool,
    fail: AtomicBool,
    assigned: Mutex<Vec<String>>,
}

impl IssueWriter for FakeWriter {
    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    async fn assign(&self, issue_id: &str) -> Result<(), String> {
        if self.fail.load(Ordering::SeqCst) {
            return Err("PostHog assign failed: 503".into());
        }
        self.assigned.lock().unwrap().push(issue_id.to_string());
        Ok(())
    }
}

#[derive(Default)]
struct World {
    source: FakeSource,
    dispatcher: FakeDispatcher,
    mailer: FakeMailer,
    writer: FakeWriter,
}

impl World {
    async fn tick(&self, pool: &MySqlPool, config: &Config, now: DateTime<Utc>) -> TickReport {
        let deps = Deps {
            pool,
            config,
            source: &self.source,
            dispatcher: &self.dispatcher,
            mailer: &self.mailer,
            writer: &self.writer,
        };
        tick(&deps, now).await.unwrap()
    }

    /// Ticks once per poll window from `from` until every issue is judged.
    async fn drain(&self, pool: &MySqlPool, config: &Config, from: i64) {
        for poll in 0..10 {
            if count(pool, "status = 'discovered'").await == 0 {
                return;
            }
            self.tick(pool, config, minutes(from + poll * 5)).await;
        }
        panic!("issues left undiscovered");
    }
}

fn live() -> Config {
    Config {
        mode: Mode::Live,
        start_at: Some(t0()),
        posthog_key: Some("phx_test".into()),
        github_token: Some("ghp_test".into()),
        recipients: vec!["dev@example.com".into()],
        ..Config::default()
    }
}

/// An error as PostHog reports it.
struct Bug<'a> {
    name: &'a str,
    description: &'a str,
    distinct_id: &'a str,
    lib: &'a str,
    /// A symbolicated top frame (`function`, `file`); `None` is minified.
    frame: Option<(&'a str, &'a str)>,
}

fn web<'a>(name: &'a str, description: &'a str, frame: Option<(&'a str, &'a str)>) -> Bug<'a> {
    Bug {
        name,
        description,
        distinct_id: "user-1",
        lib: "web",
        frame,
    }
}

fn issue(n: u32, created_at: DateTime<Utc>, bug: &Bug<'_>) -> (Issue, IssueEvents) {
    let frame = bug.frame.map_or_else(
        || json!({ "function": "a", "source": "https://granite-manager.com/assets/index-3f2a.js", "in_app": true, "resolved": false }),
        |(function, file)| json!({ "function": function, "source": file, "in_app": true, "resolved": true }),
    );
    let id = issue_id(n);
    let events = IssueEvents {
        issue_id: id.clone(),
        occurrences: 4,
        users: 2,
        distinct_id: Some(bug.distinct_id.into()),
        lib: Some(bug.lib.into()),
        host: (bug.lib == "web").then(|| "granite-manager.com".into()),
        exception_list: Some(json!([{
            "type": bug.name,
            "value": bug.description,
            "stacktrace": { "type": "resolved", "frames": [frame] },
        }])),
    };
    let issue = Issue {
        id,
        created_at,
        status: "active".into(),
        name: bug.name.into(),
        description: bug.description.into(),
    };
    (issue, events)
}

const SLABS: &str = r#"Granite API 500 on /v1/slabs?id=6074: {"error":"database error"}"#;

fn schedule_bug() -> Bug<'static> {
    Bug {
        name: "ApiError",
        description: "Database(Database(MySqlDatabaseError { code: Some(\"42S02\"), \
            message: \"Table 'main.schedule_notification_flows' doesn't exist\" }))",
        distinct_id: "granite-api",
        lib: "posthog-rs",
        frame: Some(("schedule_notifications::list", "api/src/routes/schedule.rs")),
    }
}

fn slabs_bug(description: &str) -> Bug<'_> {
    web(
        "GraniteApiError",
        description,
        Some(("fetchSlabs", "app/lib/api/slabs.ts")),
    )
}

fn field_state_bug() -> Bug<'static> {
    web(
        "TypeError",
        "Cannot read properties of undefined (reading 'getFieldState')",
        Some(("useFormField", "app/components/ui/form.tsx")),
    )
}

/// 110 issues of one network error, split by PostHog.
fn add_failed_to_fetch(source: &FakeSource, created_from: DateTime<Utc>) {
    for n in 1..=110 {
        let bug = web("TypeError", "Failed to fetch", None);
        source.add(issue(
            n,
            created_from + Duration::seconds(i64::from(n)),
            &bug,
        ));
    }
}

async fn count(pool: &MySqlPool, condition: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM posthog_autofix_runs WHERE {condition}"
    )))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn run_of(pool: &MySqlPool, n: u32) -> (u64, String) {
    sqlx::query_as(
        "SELECT id, CAST(status AS CHAR) FROM posthog_autofix_runs WHERE posthog_issue_id = ?",
    )
    .bind(issue_id(n))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn insert_run(pool: &MySqlPool, n: u32, columns: &str, values: &str) -> u64 {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO posthog_autofix_runs \
         (posthog_issue_id, issue_created_at, issue_name, issue_description, {columns}) \
         VALUES (?, '2026-10-21 00:00:00', 'TypeError', 'x is undefined', {values})"
    )))
    .bind(issue_id(n))
    .execute(pool)
    .await
    .unwrap()
    .last_insert_id()
}

#[test_db::test]
async fn a_storm_of_one_error_starts_one_run_per_real_bug(pool: MySqlPool) {
    let config = Config {
        max_concurrent: 3,
        max_per_day: 5,
        ..live()
    };
    let world = World::default();
    let source = &world.source;
    source.add(issue(900, t0() - Duration::hours(1), &schedule_bug()));
    source.add(issue(200, t0() + Duration::seconds(1), &schedule_bug()));
    source.add(issue(201, t0() + Duration::seconds(2), &slabs_bug(SLABS)));
    source.add(issue(202, t0() + Duration::seconds(3), &field_state_bug()));
    source.add(issue(203, t0() + Duration::seconds(4), &slabs_bug(SLABS)));
    let other_slab = SLABS.replace("6074", "6081");
    source.add(issue(
        204,
        t0() + Duration::seconds(5),
        &slabs_bug(&other_slab),
    ));
    add_failed_to_fetch(source, t0() + Duration::minutes(1));

    let first = world.tick(&pool, &config, minutes(5)).await;
    assert_eq!(first.discovered, 115);
    assert!(first.storm_digest);
    assert_eq!(first.dispatched, 0, "nothing has settled yet");

    let second = world.tick(&pool, &config, minutes(40)).await;
    assert_eq!(second.dispatched, 3);
    assert!(!second.storm_digest);
    let calls = source.calls();
    let same_window = world.tick(&pool, &config, minutes(41)).await;
    assert_eq!(
        source.calls(),
        calls,
        "PostHog is read once per poll window"
    );
    assert_eq!(same_window.discovered + same_window.dispatched, 0);

    world.drain(&pool, &config, 45).await;
    let dispatched = world.dispatcher.attempts();
    assert_eq!(dispatched.len(), 3);
    let real = [
        run_of(&pool, 200).await,
        run_of(&pool, 201).await,
        run_of(&pool, 202).await,
    ];
    for (id, status) in &real {
        assert!(dispatched.contains(id));
        assert_eq!(status, "dispatched");
    }
    let backlog: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM posthog_autofix_runs WHERE posthog_issue_id = ?")
            .bind(issue_id(900))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(backlog, 0, "the backlog never enters the ledger");
    assert_eq!(
        count(&pool, "status = 'skipped' AND skip_reason = 'excluded'").await,
        110
    );
    let slabs_run = real[1].0;
    assert_eq!(
        count(
            &pool,
            &format!("status = 'duplicate' AND duplicate_of_run_id = {slabs_run}")
        )
        .await,
        2
    );
    assert_eq!(count(&pool, "status = 'duplicate'").await, 2);
    let subjects = world.mailer.subjects();
    assert_eq!(subjects.len(), 1, "{subjects:?}");
    assert!(subjects[0].contains("storm"), "{subjects:?}");
}

#[test_db::test]
async fn without_the_exclusions_one_error_split_110_ways_is_one_run(pool: MySqlPool) {
    let config = Config {
        exclude: Vec::new(),
        ..live()
    };
    let world = World::default();
    add_failed_to_fetch(&world.source, t0());
    world.tick(&pool, &config, minutes(5)).await;
    world.drain(&pool, &config, 40).await;

    let dispatched = world.dispatcher.attempts();
    assert_eq!(dispatched.len(), 1);
    let run = dispatched[0];
    assert_eq!(count(&pool, "status IN ('queued', 'dispatched')").await, 1);
    assert_eq!(
        count(
            &pool,
            &format!("status = 'duplicate' AND duplicate_of_run_id = {run}")
        )
        .await,
        109
    );
}

#[test_db::test]
async fn a_run_over_the_daily_cap_waits_in_the_queue(pool: MySqlPool) {
    let config = Config {
        max_per_day: 1,
        ..live()
    };
    let world = World::default();
    world.source.add(issue(200, t0(), &schedule_bug()));
    world
        .source
        .add(issue(202, t0() + Duration::seconds(1), &field_state_bug()));
    world.tick(&pool, &config, minutes(5)).await;
    for at in [40, 45, 50, 60, 90] {
        world.tick(&pool, &config, minutes(at)).await;
    }
    assert_eq!(world.dispatcher.attempts().len(), 1);
    assert_eq!(count(&pool, "status = 'dispatched'").await, 1);
    assert_eq!(count(&pool, "status = 'queued'").await, 1, "never dropped");
}

async fn retry_at(pool: &MySqlPool) -> DateTime<Utc> {
    let at: NaiveDateTime = sqlx::query_scalar(
        "SELECT retry_at FROM posthog_autofix_runs WHERE status = 'queued' AND last_error IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    at.and_utc()
}

#[test_db::test]
async fn a_refused_dispatch_backs_off_then_fails_with_one_email(pool: MySqlPool) {
    let config = live();
    let world = World::default();
    world.dispatcher.fail.store(true, Ordering::SeqCst);
    world.source.add(issue(200, t0(), &schedule_bug()));
    world.tick(&pool, &config, minutes(5)).await;
    let first = world.tick(&pool, &config, minutes(40)).await;
    assert_eq!(first.dispatched, 0);
    assert_eq!(first.errors.len(), 1, "{:?}", first.errors);
    assert_eq!(world.dispatcher.attempts().len(), 1);

    for attempt in 2..=config.max_attempts {
        let due = retry_at(&pool).await;
        world.tick(&pool, &config, due - Duration::seconds(1)).await;
        assert_eq!(
            world.dispatcher.attempts().len(),
            usize::from(attempt - 1),
            "backoff"
        );
        world.tick(&pool, &config, due).await;
        assert_eq!(world.dispatcher.attempts().len(), usize::from(attempt));
    }
    assert_eq!(
        count(&pool, "status = 'failed' AND attempts = 3 AND last_error LIKE '%500 boom%' AND finished_at IS NOT NULL").await,
        1
    );
    assert_eq!(world.mailer.subjects().len(), 1);
    world.tick(&pool, &config, minutes(600)).await;
    assert_eq!(world.dispatcher.attempts().len(), 3);
    assert_eq!(world.mailer.subjects().len(), 1);
}

#[test_db::test]
async fn the_watchdog_times_out_a_run_that_never_reported(pool: MySqlPool) {
    let config = live();
    let world = World::default();
    let id = insert_run(
        &pool,
        1,
        "status, attempts, dispatched_at",
        "'dispatched', 1, '2026-10-20 21:00:00'",
    )
    .await;
    let recent = insert_run(
        &pool,
        2,
        "status, attempts, dispatched_at",
        "'dispatched', 1, '2026-10-20 23:30:00'",
    )
    .await;

    let report = world.tick(&pool, &config, minutes(10)).await;
    assert_eq!(report.timed_out, 1);
    assert_eq!(
        count(
            &pool,
            &format!("id = {id} AND status = 'timed_out' AND finished_at = '2026-10-21 00:10:00'")
        )
        .await,
        1
    );
    assert_eq!(
        count(&pool, &format!("id = {recent} AND status = 'dispatched'")).await,
        1
    );
    assert_eq!(world.mailer.subjects().len(), 1);
    world.tick(&pool, &config, minutes(20)).await;
    assert_eq!(world.mailer.subjects().len(), 1);
}

#[test_db::test]
async fn a_dry_run_previews_instead_of_dispatching(pool: MySqlPool) {
    let config = Config {
        mode: Mode::DryRun,
        github_token: None,
        ..live()
    };
    let world = World::default();
    world.source.add(issue(200, t0(), &schedule_bug()));
    world.tick(&pool, &config, minutes(5)).await;
    let report = world.tick(&pool, &config, minutes(40)).await;
    assert_eq!((report.queued, report.previewed), (1, 1));
    assert_eq!(run_of(&pool, 200).await.1, "previewed");
    assert_eq!(world.mailer.subjects().len(), 1);

    world.source.add(issue(203, minutes(10), &schedule_bug()));
    world.tick(&pool, &config, minutes(45)).await;
    world.tick(&pool, &config, minutes(50)).await;
    assert_eq!(
        run_of(&pool, 203).await.1,
        "duplicate",
        "a previewed bug is not previewed twice"
    );
    assert!(world.dispatcher.attempts().is_empty());
    assert_eq!(
        world.mailer.subjects().len(),
        1,
        "no digest without new previews"
    );
}

#[test_db::test]
async fn a_tick_while_another_holds_the_lock_is_busy(pool: MySqlPool) {
    let config = live();
    let world = World::default();
    let mut holder = pool.acquire().await.unwrap();
    let held: i64 =
        sqlx::query_scalar("SELECT GET_LOCK(CONCAT('posthog_autofix_tick:', DATABASE()), 0)")
            .fetch_one(&mut *holder)
            .await
            .unwrap();
    assert_eq!(held, 1);

    let report = world.tick(&pool, &config, minutes(5)).await;
    assert!(report.busy);
    assert_eq!(world.source.calls(), 0);

    sqlx::query("SELECT RELEASE_LOCK(CONCAT('posthog_autofix_tick:', DATABASE()))")
        .execute(&mut *holder)
        .await
        .unwrap();
    assert!(!world.tick(&pool, &config, minutes(5)).await.busy);
}

#[test_db::test]
async fn a_reopened_fix_is_emailed_once(pool: MySqlPool) {
    let config = live();
    let world = World::default();
    let id = insert_run(
        &pool,
        1,
        "status, outcome, finished_at, last_issue_status",
        "'succeeded', 'fixed_with_test', '2026-10-20 00:00:00', 'resolved'",
    )
    .await;
    let (mut reopened, events) = issue(1, t0() - Duration::days(2), &field_state_bug());
    reopened.status = "active".into();
    world.source.add((reopened, events));

    let report = world.tick(&pool, &config, minutes(10)).await;
    assert_eq!(report.regressions, 1);
    assert_eq!(world.mailer.subjects().len(), 1);
    assert_eq!(
        count(
            &pool,
            &format!(
                "id = {id} AND last_issue_status = 'active' AND regression_notified_at IS NOT NULL"
            )
        )
        .await,
        1
    );
    let next_poll = world.tick(&pool, &config, minutes(20)).await;
    assert_eq!(next_poll.regressions, 0);
    assert_eq!(world.mailer.subjects().len(), 1);
}

#[test_db::test]
async fn an_opened_fix_is_assigned_in_posthog_once(pool: MySqlPool) {
    let config = live();
    let world = World::default();
    let opened = |n: u32| {
        insert_run(
            &pool,
            n,
            "status, outcome, pr_url, finished_at",
            "'succeeded', 'fixed_with_test', 'https://github.com/o/r/pull/1', '2026-10-20 12:00:00'",
        )
    };
    opened(1).await;
    // Write-back off: nothing is sent.
    assert_eq!(world.tick(&pool, &config, minutes(10)).await.assigned, 0);

    world.writer.enabled.store(true, Ordering::SeqCst);
    world.writer.fail.store(true, Ordering::SeqCst);
    let failed = world.tick(&pool, &config, minutes(20)).await;
    assert_eq!(failed.assigned, 0);
    assert_eq!(failed.errors.len(), 1);

    world.writer.fail.store(false, Ordering::SeqCst);
    assert_eq!(world.tick(&pool, &config, minutes(30)).await.assigned, 1);
    assert_eq!(world.tick(&pool, &config, minutes(40)).await.assigned, 0);
    assert_eq!(*world.writer.assigned.lock().unwrap(), [issue_id(1)]);
}

#[test_db::test]
async fn off_and_unconfigured_ticks_touch_nothing(pool: MySqlPool) {
    let world = World::default();
    world.source.add(issue(200, t0(), &schedule_bug()));
    let off = world.tick(&pool, &Config::default(), minutes(5)).await;
    assert_eq!(off, TickReport::default());
    let unconfigured = Config {
        posthog_key: None,
        ..live()
    };
    let report = world.tick(&pool, &unconfigured, minutes(5)).await;
    assert_eq!(report.errors, ["POSTHOG_AUTOFIX_POSTHOG_KEY is not set"]);
    assert_eq!(world.source.calls(), 0);
    assert_eq!(count(&pool, "1").await, 0);
}
