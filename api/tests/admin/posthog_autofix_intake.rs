//! `posthog_autofix::intake::tick` against fake PostHog, GitHub and mail.

use api::posthog_autofix::config::{Config, Mode};
use api::posthog_autofix::github::{Dispatch, DispatchError, Dispatcher};
use api::posthog_autofix::intake::{Deps, TickReport, tick};
use api::posthog_autofix::notify::{Email, Mailer};
use api::posthog_autofix::posthog::{EventGroup, Issue, IssueEvents, IssueSource, SourceError};
use api::posthog_autofix::writeback::IssueWriter;
use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::BTreeMap;
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

/// One `$exception` event, as PostHog stores it.
#[derive(Debug, Clone)]
struct Event {
    uuid: String,
    issue_id: String,
    at: DateTime<Utc>,
    distinct_id: String,
    bug: Bug,
}

/// An error as one event reports it.
#[derive(Debug, Clone)]
struct Bug {
    name: &'static str,
    message: String,
    /// The browser's user, or the server's fixed id.
    distinct_id: String,
    lib: Option<&'static str>,
    /// A symbolicated top frame (`function`, `file`); `None` is minified.
    frame: Option<(&'static str, &'static str)>,
    api_status: Option<u16>,
    route: Option<String>,
    error_source: Option<&'static str>,
    host: Option<&'static str>,
}

impl Bug {
    fn is_server(&self) -> bool {
        matches!(
            self.distinct_id.as_str(),
            "granite-api" | "server-webhooks" | "server-time-triggered"
        )
    }

    fn exception_list(&self) -> Value {
        let frame = self.frame.map_or_else(
            || json!({ "function": "a", "source": "https://granite-manager.com/assets/index-3f2a.js", "in_app": true, "resolved": false }),
            |(function, file)| json!({ "function": function, "source": file, "in_app": true, "resolved": true }),
        );
        json!([{
            "type": self.name,
            "value": self.message,
            "stacktrace": { "type": "resolved", "frames": [frame] },
        }])
    }
}

fn web(name: &'static str, message: &str) -> Bug {
    Bug {
        name,
        message: message.into(),
        distinct_id: "user-1".into(),
        lib: Some("web"),
        frame: None,
        api_status: None,
        route: None,
        error_source: Some("client"),
        host: Some("acme.granite-manager.com"),
    }
}

fn web_api(status: u16, path: &str, body: &str) -> Bug {
    Bug {
        api_status: Some(status),
        route: Some(path.into()),
        ..web(
            "GraniteApiError",
            &format!("Granite API {status} on {path}: {body}"),
        )
    }
}

fn api(message: &str, route: &str) -> Bug {
    Bug {
        name: "ApiError",
        message: message.into(),
        distinct_id: "granite-api".into(),
        lib: None,
        frame: None,
        api_status: Some(500),
        route: Some(route.into()),
        error_source: Some("api"),
        host: None,
    }
}

fn user(bug: Bug, distinct_id: &str) -> Bug {
    Bug {
        distinct_id: distinct_id.into(),
        ..bug
    }
}

fn schedule_bug() -> Bug {
    api(
        "Database(Database(MySqlDatabaseError { code: Some(\"42S02\"), number: 1146, \
         message: \"Table 'main.schedule_notification_flows' doesn't exist\" }))",
        "/v1/schedule/notifications",
    )
}

const UNKNOWN_COLUMN: &str = "Database(Database(MySqlDatabaseError { code: Some(\"42S22\"), \
    number: 1054, message: \"Unknown column 'co.estimates_calendar_id' in 'where clause'\" }))";
const DATABASE_ERROR: &str = r#"{"error":"database error"}"#;

fn slabs_bug(id: u32) -> Bug {
    web_api(500, &format!("/v1/slabs?id={id}"), DATABASE_ERROR)
}

fn field_state_bug() -> Bug {
    Bug {
        frame: Some(("useFormField", "app/components/ui/form.tsx")),
        ..web(
            "TypeError",
            "Cannot read properties of undefined (reading 'getFieldState')",
        )
    }
}

/// A distinct bug for each `n` (the quoted name is kept by the signature).
fn numbered_bug(n: u32) -> Bug {
    let letter = |k: u32| char::from_u32('a' as u32 + k % 26).unwrap();
    web(
        "TypeError",
        &format!(
            "Cannot read properties of undefined (reading 'field{}{}')",
            letter(n / 26),
            letter(n)
        ),
    )
}

#[derive(Default)]
struct FakeSource {
    issues: Mutex<Vec<Issue>>,
    events: Mutex<Vec<Event>>,
    /// The spans `event_groups` was asked for.
    reads: Mutex<Vec<(DateTime<Utc>, DateTime<Utc>)>>,
    fail: AtomicBool,
    calls: AtomicUsize,
}

impl FakeSource {
    /// A PostHog issue whose title is `bug`'s, created at `created_at`.
    fn issue(&self, n: u32, created_at: DateTime<Utc>, bug: &Bug) {
        self.issues.lock().unwrap().push(Issue {
            id: issue_id(n),
            created_at,
            status: "active".into(),
            name: bug.name.into(),
            description: bug.message.clone(),
        });
    }

    /// An event of `bug` in issue `n` at `at` (the issue is made on its
    /// first event); returns the event's uuid.
    fn event(&self, n: u32, at: DateTime<Utc>, bug: &Bug) -> String {
        let id = issue_id(n);
        if !self
            .issues
            .lock()
            .unwrap()
            .iter()
            .any(|issue| issue.id == id)
        {
            self.issue(n, at, bug);
        }
        let mut events = self.events.lock().unwrap();
        let uuid = format!("01a17880-0000-7000-8000-{:012x}", events.len() + 1);
        events.push(Event {
            uuid: uuid.clone(),
            issue_id: id,
            at,
            distinct_id: bug.distinct_id.clone(),
            bug: bug.clone(),
        });
        uuid
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn reads(&self) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
        self.reads.lock().unwrap().clone()
    }
}

type GroupKey = (
    String,
    &'static str,
    String,
    Option<String>,
    Option<&'static str>,
    Option<u16>,
    Option<String>,
    Option<&'static str>,
);

impl IssueSource for FakeSource {
    async fn event_groups(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<EventGroup>, SourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(SourceError("PostHog query failed: 503".into()));
        }
        self.reads.lock().unwrap().push((from, to));
        let mut groups: BTreeMap<GroupKey, Vec<Event>> = BTreeMap::new();
        for event in self.events.lock().unwrap().iter() {
            if event.at < from || event.at >= to {
                continue;
            }
            let bug = &event.bug;
            let key = (
                event.issue_id.clone(),
                bug.name,
                bug.message.clone(),
                bug.is_server().then(|| bug.distinct_id.clone()),
                bug.lib,
                bug.api_status,
                bug.route.clone(),
                bug.error_source,
            );
            groups.entry(key).or_default().push(event.clone());
        }
        let mut found: Vec<EventGroup> = groups
            .into_iter()
            .map(|(key, events)| {
                let latest = events.iter().max_by_key(|event| event.at).unwrap();
                let mut users: Vec<&str> = events.iter().map(|e| e.distinct_id.as_str()).collect();
                users.sort_unstable();
                users.dedup();
                EventGroup {
                    issue_id: key.0,
                    exception_type: key.1.into(),
                    message: key.2,
                    sender: key.3,
                    lib: key.4.map(Into::into),
                    api_status: key.5,
                    route: key.6,
                    error_source: key.7.map(Into::into),
                    occurrences: events.len() as u64,
                    users: users.len() as u64,
                    first_seen: events.iter().map(|event| event.at).min().unwrap(),
                    last_seen: latest.at,
                    event_uuid: Some(latest.uuid.clone()),
                    host: latest.bug.host.map(Into::into),
                    exception_list: Some(latest.bug.exception_list()),
                    url: None,
                    method: None,
                }
            })
            .collect();
        found.sort_by_key(|group| std::cmp::Reverse(group.occurrences));
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
        since: DateTime<Utc>,
    ) -> Result<Vec<IssueEvents>, SourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events = self.events.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| {
                let of_issue: Vec<&Event> = events
                    .iter()
                    .filter(|event| &event.issue_id == id && event.at >= since)
                    .collect();
                let latest = of_issue.iter().max_by_key(|event| event.at)?;
                let web = latest.bug.lib == Some("web");
                Some(IssueEvents {
                    issue_id: id.clone(),
                    occurrences: of_issue.len() as u64,
                    users: 1,
                    distinct_id: Some(latest.distinct_id.clone()),
                    lib: latest.bug.lib.map(Into::into),
                    host: latest.bug.host.map(Into::into),
                    exception_list: Some(latest.bug.exception_list()),
                    url: web.then(|| "https://granite-manager.com/employee/deals/7".into()),
                    request: latest
                        .bug
                        .route
                        .as_ref()
                        .map(|route| format!("GET {route}")),
                })
            })
            .collect())
    }
}

#[derive(Default)]
struct FakeDispatcher {
    /// `(run id, issue id, event id)` of every dispatch attempt.
    attempts: Mutex<Vec<(u64, String, Option<String>)>>,
    fail: AtomicBool,
}

impl FakeDispatcher {
    fn attempts(&self) -> Vec<u64> {
        self.attempts
            .lock()
            .unwrap()
            .iter()
            .map(|(run, _, _)| *run)
            .collect()
    }
}

impl Dispatcher for FakeDispatcher {
    async fn dispatch(&self, dispatch: &Dispatch<'_>) -> Result<(), DispatchError> {
        self.attempts.lock().unwrap().push((
            dispatch.run_id,
            dispatch.issue_id.to_string(),
            dispatch.event_id.map(Into::into),
        ));
        if self.fail.load(Ordering::SeqCst) {
            return Err(DispatchError("GitHub dispatch failed: 500 boom".into()));
        }
        Ok(())
    }
}

#[derive(Default)]
struct FakeMailer {
    sent: Mutex<Vec<Email>>,
    /// When set, each send records whether the tick's lock was free.
    pool: std::sync::OnceLock<MySqlPool>,
    lock_free: Mutex<Vec<bool>>,
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
        if let Some(pool) = self.pool.get() {
            let free: Option<i64> = sqlx::query_scalar(
                "SELECT IS_FREE_LOCK(CONCAT('posthog_autofix_tick:', DATABASE()))",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            self.lock_free.lock().unwrap().push(free == Some(1));
        }
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

    async fn assign(&self, _run_id: u64, issue_id: &str) -> Result<(), String> {
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

    /// Ticks once per poll window from `from` until every bug is judged.
    async fn drain(&self, pool: &MySqlPool, config: &Config, from: i64) {
        for poll in 0..10 {
            if count(pool, "status = 'discovered'").await == 0 {
                return;
            }
            self.tick(pool, config, minutes(from + poll * 5)).await;
        }
        panic!("bugs left unjudged");
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

async fn count(pool: &MySqlPool, condition: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM posthog_autofix_runs WHERE {condition}"
    )))
    .fetch_one(pool)
    .await
    .unwrap()
}

/// `(id, status, skip_reason)` of the row of the bug whose message starts so.
async fn row_of(pool: &MySqlPool, message: &str) -> (u64, String, Option<String>) {
    sqlx::query_as(
        "SELECT id, CAST(status AS CHAR), skip_reason FROM posthog_autofix_runs \
         WHERE issue_description LIKE CONCAT(?, '%')",
    )
    .bind(message)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn counters(pool: &MySqlPool, id: u64) -> (u32, u32) {
    sqlx::query_as("SELECT occurrences, users FROM posthog_autofix_runs WHERE id = ?")
        .bind(id)
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
async fn one_bug_split_over_many_issues_is_one_row(pool: MySqlPool) {
    let config = Config {
        max_concurrent: 3,
        max_per_day: 5,
        ..live()
    };
    let world = World::default();
    let source = &world.source;
    // Before the cutover: never read.
    source.event(900, t0() - Duration::hours(1), &schedule_bug());
    source.event(200, t0() + Duration::seconds(1), &schedule_bug());
    for (n, id) in [(201, 6074), (203, 6074), (204, 6081)] {
        source.event(
            n,
            t0() + Duration::seconds(i64::from(n - 195)),
            &slabs_bug(id),
        );
    }
    source.event(202, t0() + Duration::seconds(3), &field_state_bug());
    // 110 issues of one network error, split by PostHog.
    for n in 1..=110 {
        let at = minutes(1) + Duration::seconds(i64::from(n));
        source.event(n, at, &web("TypeError", "Failed to fetch"));
    }

    let first = world.tick(&pool, &config, minutes(5)).await;
    assert_eq!(first.discovered, 4);
    assert_eq!(first.dispatched, 0, "nothing has settled yet");

    let second = world.tick(&pool, &config, minutes(40)).await;
    assert_eq!(second.dispatched, 3);
    let calls = source.calls();
    let same_window = world.tick(&pool, &config, minutes(41)).await;
    assert_eq!(
        source.calls(),
        calls,
        "PostHog is read once per poll window"
    );
    assert_eq!(same_window.discovered + same_window.dispatched, 0);

    assert_eq!(count(&pool, "1").await, 4);
    assert_eq!(count(&pool, "status = 'dispatched'").await, 3);
    let (_, status, reason) = row_of(&pool, "Failed to fetch").await;
    assert_eq!(
        (status.as_str(), reason.as_deref()),
        ("skipped", Some("excluded"))
    );
    let (slabs, _, _) = row_of(&pool, "Granite API 500 on /v1/slabs").await;
    assert_eq!(counters(&pool, slabs).await.0, 3);
    let fetch_events: (u32,) = sqlx::query_as(
        "SELECT occurrences FROM posthog_autofix_runs WHERE issue_description = 'Failed to fetch'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(fetch_events.0, 110);
    assert!(world.mailer.subjects().is_empty());
}

#[test_db::test]
async fn without_the_exclusions_one_error_split_110_ways_is_one_run(pool: MySqlPool) {
    let config = Config {
        exclude: Vec::new(),
        ..live()
    };
    let world = World::default();
    for n in 1..=110 {
        let at = t0() + Duration::seconds(i64::from(n));
        world
            .source
            .event(n, at, &web("TypeError", "Failed to fetch"));
    }
    world.tick(&pool, &config, minutes(5)).await;
    world.drain(&pool, &config, 40).await;
    assert_eq!(world.dispatcher.attempts().len(), 1);
    assert_eq!(count(&pool, "1").await, 1);
}

#[test_db::test]
async fn a_storm_across_a_window_boundary_sends_one_digest(pool: MySqlPool) {
    let config = live();
    let world = World::default();
    for n in 0..25 {
        let at = minutes(20) + Duration::seconds(i64::from(n));
        world.source.event(n, at, &numbered_bug(n));
    }

    // 00:29 is in window 00:00, 00:31 in window 00:30: the same 25 bugs.
    assert!(world.tick(&pool, &config, minutes(29)).await.storm_digest);
    assert!(!world.tick(&pool, &config, minutes(31)).await.storm_digest);
    assert_eq!(world.mailer.subjects().len(), 1);

    // A new storm more than a storm window after the first digest is a new digest.
    for n in 100..125 {
        let at = minutes(100) + Duration::seconds(i64::from(n));
        world.source.event(n, at, &numbered_bug(n));
    }
    assert!(world.tick(&pool, &config, minutes(105)).await.storm_digest);
    assert_eq!(world.mailer.subjects().len(), 2);
}

#[test_db::test]
async fn a_catch_all_issue_titled_401_holds_a_500_that_gets_its_own_run(pool: MySqlPool) {
    let config = live();
    let world = World::default();
    let source = &world.source;
    let signed_out = web_api(401, "/v1/me", r#"{"error":"unauthorized"}"#);
    // The issue predates the cutover and its title is a signed-out 401.
    source.issue(1, t0() - Duration::days(9), &signed_out);
    source.event(1, minutes(1), &signed_out);
    let customers = web_api(500, "/v1/customers", DATABASE_ERROR);
    source.event(1, minutes(2), &customers);
    source.event(1, minutes(2), &user(customers.clone(), "user-2"));
    // A developer's browser: never recorded.
    let local = Bug {
        host: Some("localhost:5173"),
        ..web("TypeError", "x is not a function")
    };
    source.event(5, minutes(2), &local);

    world.tick(&pool, &config, minutes(5)).await;
    world.drain(&pool, &config, 40).await;

    assert_eq!(count(&pool, "1").await, 2);
    let (_, status, reason) = row_of(&pool, "Granite API 401 on /v1/me").await;
    assert_eq!(
        (status.as_str(), reason.as_deref()),
        ("skipped", Some("excluded"))
    );
    let (id, status, _) = row_of(&pool, "Granite API 500 on /v1/customers").await;
    assert_eq!(status, "dispatched");
    assert_eq!(counters(&pool, id).await, (2, 2));
    let attempts = world.dispatcher.attempts.lock().unwrap().clone();
    assert_eq!(attempts.len(), 1);
    assert_eq!(
        attempts[0].1,
        issue_id(1),
        "the catch-all issue is the sample"
    );
}

#[test_db::test]
async fn a_new_message_in_an_issue_from_before_the_cutover_is_queued(pool: MySqlPool) {
    let config = live();
    let world = World::default();
    let source = &world.source;
    source.issue(2, t0() - Duration::days(6), &schedule_bug());
    source.event(2, t0() - Duration::days(6), &schedule_bug());
    let unknown_column = api(UNKNOWN_COLUMN, "/v1/schedule/settings");
    source.event(2, minutes(1), &unknown_column);

    world.tick(&pool, &config, minutes(5)).await;
    let report = world.tick(&pool, &config, minutes(40)).await;
    assert_eq!((report.queued, report.dispatched), (1, 1));
    assert_eq!(count(&pool, "1").await, 1, "the old message is never read");
    let (_, status, _) = row_of(
        &pool,
        "Database(Database(MySqlDatabaseError { code: Some(\"42S22\")",
    )
    .await;
    assert_eq!(status, "dispatched");
}

#[test_db::test]
async fn a_web_500_folds_into_the_api_error_of_its_route(pool: MySqlPool) {
    let config = live();
    let world = World::default();
    let source = &world.source;
    // The api's issue is old; the browser's errors split over two issues.
    source.issue(3, t0() - Duration::days(6), &schedule_bug());
    let server = api(UNKNOWN_COLUMN, "/v1/schedule/settings");
    let browser = web_api(500, "/v1/schedule/settings", DATABASE_ERROR);
    for (n, who) in ["101", "102", "103", "104"].into_iter().enumerate() {
        let at = minutes(1) + Duration::seconds(i64::try_from(n).unwrap() * 30);
        source.event(3, at, &server);
        let issue = if n < 2 { 10 } else { 11 };
        source.event(
            issue,
            at + Duration::milliseconds(400),
            &user(browser.clone(), who),
        );
    }

    world.tick(&pool, &config, minutes(5)).await;
    let report = world.tick(&pool, &config, minutes(40)).await;
    assert_eq!(
        (report.queued, report.duplicates, report.dispatched),
        (1, 1, 1)
    );

    let (api_row, status, _) = row_of(&pool, "Database(Database(").await;
    assert_eq!(status, "dispatched");
    let (web_row, status, _) = row_of(&pool, "Granite API 500 on /v1/schedule/settings").await;
    assert_eq!(status, "duplicate");
    // Two issues of two users each: users is the widest group's.
    assert_eq!(counters(&pool, web_row).await, (4, 2));
    assert_eq!(
        counters(&pool, api_row).await,
        (4, 2),
        "users lifted to the browser's"
    );
    assert_eq!(
        count(
            &pool,
            &format!("id = {web_row} AND duplicate_of_run_id = {api_row}")
        )
        .await,
        1
    );
    assert_eq!(world.dispatcher.attempts(), [api_row]);

    // A later 500 on another route stands on its own.
    source.event(
        12,
        minutes(41),
        &web_api(500, "/v1/deals?group_id=1", DATABASE_ERROR),
    );
    world.tick(&pool, &config, minutes(45)).await;
    world.drain(&pool, &config, 80).await;
    let (_, status, _) = row_of(&pool, "Granite API 500 on /v1/deals").await;
    assert_eq!(status, "dispatched");
}

#[test_db::test]
async fn polls_count_each_event_once_and_a_failed_read_is_read_again(pool: MySqlPool) {
    let config = live();
    let world = World::default();
    let source = &world.source;
    let bug = field_state_bug();
    let at = |minute: i64, second: i64| minutes(minute) + Duration::seconds(second);
    source.event(1, at(1, 0), &bug);
    // Within the last two minutes of the first poll: left for the next one.
    source.event(1, at(3, 30), &bug);
    world.tick(&pool, &config, minutes(5)).await;
    let (id, _, _) = row_of(&pool, "Cannot read properties").await;
    assert_eq!(counters(&pool, id).await.0, 1);

    source.event(2, at(6, 0), &user(bug.clone(), "user-2"));
    world.tick(&pool, &config, minutes(10)).await;
    assert_eq!(counters(&pool, id).await, (3, 1));

    source.fail.store(true, Ordering::SeqCst);
    source.event(2, at(9, 0), &bug);
    let failed = world.tick(&pool, &config, minutes(15)).await;
    assert_eq!(failed.errors.len(), 1, "{:?}", failed.errors);
    source.fail.store(false, Ordering::SeqCst);
    world.tick(&pool, &config, minutes(20)).await;
    assert_eq!(counters(&pool, id).await.0, 4);

    let reads = source.reads();
    let spans: Vec<(i64, i64)> = reads
        .iter()
        .map(|(from, to)| ((*from - t0()).num_minutes(), (*to - t0()).num_minutes()))
        .collect();
    assert_eq!(spans, [(0, 3), (3, 8), (8, 18)], "no gap, no overlap");
    assert_eq!(count(&pool, "1").await, 1);
}

#[test_db::test]
async fn a_first_read_goes_back_one_day_at_most(pool: MySqlPool) {
    let config = Config {
        start_at: Some(t0() - Duration::days(3)),
        ..live()
    };
    let world = World::default();
    world
        .source
        .event(1, t0() - Duration::days(2), &numbered_bug(1));
    world
        .source
        .event(2, t0() - Duration::hours(1), &numbered_bug(2));
    let report = world.tick(&pool, &config, minutes(5)).await;
    assert_eq!(report.discovered, 1);
    assert_eq!(world.source.reads()[0].0, minutes(5) - Duration::days(1));
}

#[test_db::test]
async fn a_dispatch_names_the_latest_event_of_the_bug(pool: MySqlPool) {
    let config = live();
    let world = World::default();
    let bug = field_state_bug();
    world.source.event(1, minutes(1), &bug);
    let latest = world.source.event(2, minutes(2), &bug);
    world.tick(&pool, &config, minutes(5)).await;
    world.tick(&pool, &config, minutes(40)).await;
    let attempts = world.dispatcher.attempts.lock().unwrap().clone();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].1, issue_id(2));
    assert_eq!(attempts[0].2.as_deref(), Some(latest.as_str()));
}

#[test_db::test]
async fn the_queue_starts_bugs_seen_today_before_older_ones(pool: MySqlPool) {
    let config = Config {
        max_per_day: 1,
        ..live()
    };
    let world = World::default();
    let source = &world.source;
    for who in ["user-1", "user-2", "user-3"] {
        source.event(1, minutes(1), &user(field_state_bug(), who));
    }
    for who in ["user-1", "user-2"] {
        source.event(2, minutes(1), &user(numbered_bug(2), who));
    }
    source.event(3, minutes(1), &schedule_bug());
    world.tick(&pool, &config, minutes(5)).await;
    for at in [40, 45, 50, 60, 90] {
        world.tick(&pool, &config, minutes(at)).await;
    }
    assert_eq!(world.dispatcher.attempts().len(), 1);
    let (widest, _, _) = row_of(
        &pool,
        "Cannot read properties of undefined (reading 'getFieldState')",
    )
    .await;
    assert_eq!(
        world.dispatcher.attempts(),
        [widest],
        "three users before two or one"
    );
    assert_eq!(count(&pool, "status = 'queued'").await, 2, "never dropped");

    // The one-user bug happens again while the day's run is spent: it goes
    // next, ahead of a wider bug that stopped.
    world.tick(&pool, &config, t0() + Duration::hours(12)).await;
    source.event(3, t0() + Duration::hours(20), &schedule_bug());
    let capped = world
        .tick(&pool, &config, t0() + Duration::minutes(20 * 60 + 5))
        .await;
    assert_eq!(capped.dispatched, 0);
    let next = world
        .tick(&pool, &config, t0() + Duration::minutes(24 * 60 + 45))
        .await;
    assert_eq!(next.dispatched, 1);
    let (_, status, _) = row_of(&pool, "Database(Database(").await;
    assert_eq!(status, "dispatched");
    let (_, status, _) = row_of(
        &pool,
        "Cannot read properties of undefined (reading 'fieldac')",
    )
    .await;
    assert_eq!(status, "queued");
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
    world.source.event(200, t0(), &schedule_bug());
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
async fn emails_go_out_after_the_tick_releases_its_lock(pool: MySqlPool) {
    let config = live();
    let world = World::default();
    world.mailer.pool.set(pool.clone()).unwrap();
    insert_run(
        &pool,
        1,
        "status, attempts, dispatched_at",
        "'dispatched', 1, '2026-10-20 21:00:00'",
    )
    .await;
    let report = world.tick(&pool, &config, minutes(10)).await;
    assert_eq!(report.timed_out, 1);
    assert_eq!(*world.mailer.lock_free.lock().unwrap(), [true]);
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
    world.source.event(200, t0(), &schedule_bug());
    world.tick(&pool, &config, minutes(5)).await;
    let report = world.tick(&pool, &config, minutes(40)).await;
    assert_eq!((report.queued, report.previewed), (1, 1));
    let (id, status, _) = row_of(&pool, "Database(Database(").await;
    assert_eq!(status, "previewed");
    assert_eq!(world.mailer.subjects().len(), 1);

    // The same bug again, in another issue: counted, not previewed twice.
    world.source.event(203, minutes(41), &schedule_bug());
    world.tick(&pool, &config, minutes(45)).await;
    world.tick(&pool, &config, minutes(80)).await;
    assert_eq!(count(&pool, "1").await, 1);
    assert_eq!(counters(&pool, id).await.0, 2);
    assert!(world.dispatcher.attempts().is_empty());
    assert_eq!(
        world.mailer.subjects().len(),
        1,
        "no digest without new previews"
    );
}

#[test_db::test(seed)]
async fn a_dry_run_email_names_the_page_the_user_and_the_company(pool: MySqlPool) {
    let config = Config {
        mode: Mode::DryRun,
        github_token: None,
        ..live()
    };
    let world = World::default();
    // The seed's user 103 is Sam Super, of company 101.
    world
        .source
        .event(204, t0(), &user(field_state_bug(), "103"));
    world.tick(&pool, &config, minutes(5)).await;
    let report = world.tick(&pool, &config, minutes(40)).await;
    assert_eq!(report.previewed, 1);
    let sent = world.mailer.sent.lock().unwrap();
    assert_eq!(sent[0].subject, "[AI fix] Dry run: 1 new error");
    let html = &sent[0].html;
    assert!(html.contains("Cannot read properties of undefined (reading 'getFieldState')"));
    assert!(html.contains("<a href=\"https://granite-manager.com/employee/deals/7\">"));
    assert!(html.contains("<strong>User:</strong> Sam Super (user 103)"));
    assert!(html.contains("<strong>Company ID:</strong> 101"));
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
    world
        .source
        .issue(1, t0() - Duration::days(2), &field_state_bug());

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
    world.source.event(200, t0(), &schedule_bug());
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
