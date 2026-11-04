use crate::common;

use api::app;
use api::posthog_autofix::{Autofix, Config};
use api::state::AppState;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

const ISSUE: &str = "01a158ec-b8d5-7002-bb99-07e4e724acee";
const OTHER_ISSUE: &str = "01a158ec-b8d5-7002-bb99-07e4e724acff";
const AUTH: &str = "Bearer callback-secret";
const RUN_URL: &str = "https://github.com/o/r/actions/runs/9";
const PR_URL: &str = "https://github.com/o/r/pull/7";

type Sent = Arc<Mutex<Vec<OutgoingEmail>>>;

async fn server(pool: &MySqlPool) -> (TestServer, Sent) {
    let sent = Sent::default();
    let router = Router::new()
        .route(
            "/email/send",
            post(
                |State(sent): State<Sent>, Json(email): Json<OutgoingEmail>| async move {
                    sent.lock().unwrap().push(email);
                    Json(json!({ "message_id": "m-1" }))
                },
            ),
        )
        .with_state(sent.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let autofix = Autofix::new(Config {
        callback_secret: Some("callback-secret".into()),
        recipients: vec!["dev@example.com".into()],
        ..Config::default()
    });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_webhooks(WebhooksClient::new(
            Some(&format!("http://{address}")),
            Some("key"),
        ))
        .with_extension(autofix);
    (TestServer::new(app(state)), sent)
}

async fn insert_run(pool: &MySqlPool, status: &str, attempts: u8) -> u64 {
    sqlx::query(
        "INSERT INTO posthog_autofix_runs \
         (posthog_issue_id, issue_created_at, issue_name, issue_description, source, \
          occurrences, users, status, attempts, started_at) \
         VALUES (?, '2026-10-21 10:00:00', 'TypeError: x is null', 'at <b>foo</b>', 'web', \
          12, 3, ?, ?, '2026-10-21 10:05:00')",
    )
    .bind(ISSUE)
    .bind(status)
    .bind(attempts)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_id()
}

async fn row(pool: &MySqlPool, id: u64) -> Value {
    let (status, outcome, retry, error, notified, finished, started): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        bool,
        bool,
        bool,
    ) = sqlx::query_as(
        "SELECT CAST(status AS CHAR), CAST(outcome AS CHAR), CAST(retry_at AS CHAR), \
         last_error, notified_at IS NOT NULL, finished_at IS NOT NULL, started_at IS NOT NULL \
         FROM posthog_autofix_runs WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    json!({ "status": status, "outcome": outcome, "retry_at": retry, "last_error": error,
        "notified": notified, "finished": finished, "started": started })
}

fn completed() -> Value {
    json!({
        "issue_id": ISSUE,
        "conclusion": "completed",
        "agent": {
            "outcome": "fixed_with_test", "target_repo": "granite-webhooks",
            "summary": "Crash", "given": "a deal without a customer",
            "when": "a user opens /employee/deals/7", "then": "GET /v1/deals/7 answers 500",
            "actual": "the page shows an error", "expected": "the deal opens",
            "impact": "The deal cannot be opened.", "check": "Repeat Given/When: it opens.",
            "root_cause": "A null", "fix_summary": "Guard it",
            "risk": "Low",
            "test": { "status": "written", "files": ["api/tests/x.rs"],
                      "command": "cargo test -p api --test x" },
        },
        "verification": "verified", "pr_url": PR_URL, "cost_usd": 1.234,
        "github_run_url": RUN_URL,
    })
}

fn bare(conclusion: &str, notes: &str) -> Value {
    json!({ "issue_id": ISSUE, "conclusion": conclusion, "verification": "not_applicable",
            "notes": notes })
}

fn url(id: u64, kind: &str) -> String {
    format!("/v1/posthog-autofix/runs/{id}/{kind}")
}

#[test_db::test]
async fn callbacks_need_the_secret(pool: MySqlPool) {
    let id = insert_run(&pool, "dispatched", 1).await;
    let (server, _) = server(&pool).await;
    let body = json!({ "issue_id": ISSUE, "github_run_url": RUN_URL });
    server
        .post(&url(id, "started"))
        .json(&body)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post(&url(id, "started"))
        .add_header("Authorization", "Bearer wrong")
        .json(&body)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post(&url(id, "result"))
        .add_header("Authorization", "callback-secret")
        .json(&completed())
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(row(&pool, id).await["status"], "dispatched");
}

#[test_db::test]
async fn started_moves_dispatched_to_running_once(pool: MySqlPool) {
    let id = insert_run(&pool, "dispatched", 1).await;
    let (server, _) = server(&pool).await;
    let body = json!({ "issue_id": ISSUE, "github_run_url": RUN_URL });
    let call = || {
        server
            .post(&url(id, "started"))
            .add_header("Authorization", AUTH)
            .json(&body)
    };
    let first: Value = call().await.json();
    assert_eq!(first, json!({ "changed": true }));
    assert_eq!(row(&pool, id).await["status"], "running");
    let again = call().await;
    again.assert_status_ok();
    assert_eq!(again.json::<Value>(), json!({ "changed": false }));
}

#[test_db::test]
async fn started_refuses_a_run_that_was_not_dispatched(pool: MySqlPool) {
    let id = insert_run(&pool, "queued", 0).await;
    let (server, _) = server(&pool).await;
    server
        .post(&url(id, "started"))
        .add_header("Authorization", AUTH)
        .json(&json!({ "issue_id": ISSUE, "github_run_url": RUN_URL }))
        .await
        .assert_status(StatusCode::CONFLICT);
    assert_eq!(row(&pool, id).await["status"], "queued");
}

#[test_db::test]
async fn an_issue_id_that_differs_is_not_found(pool: MySqlPool) {
    let id = insert_run(&pool, "dispatched", 1).await;
    let (server, _) = server(&pool).await;
    server
        .post(&url(id, "started"))
        .add_header("Authorization", AUTH)
        .json(&json!({ "issue_id": OTHER_ISSUE, "github_run_url": RUN_URL }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .post(&url(id + 100, "started"))
        .add_header("Authorization", AUTH)
        .json(&json!({ "issue_id": ISSUE, "github_run_url": RUN_URL }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test]
async fn a_completed_result_succeeds_and_emails_once(pool: MySqlPool) {
    let id = insert_run(&pool, "running", 1).await;
    let (server, sent) = server(&pool).await;
    let call = || {
        server
            .post(&url(id, "result"))
            .add_header("Authorization", AUTH)
            .json(&completed())
    };
    assert_eq!(call().await.json::<Value>(), json!({ "changed": true }));
    let row_now = row(&pool, id).await;
    assert_eq!(row_now["status"], "succeeded");
    assert_eq!(row_now["outcome"], "fixed_with_test");
    assert_eq!(row_now["notified"], true);
    let (cost, pr): (String, String) = sqlx::query_as(
        "SELECT CAST(cost_usd AS CHAR), pr_url FROM posthog_autofix_runs WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((cost.as_str(), pr.as_str()), ("1.23", PR_URL));
    {
        let emails = sent.lock().unwrap();
        assert_eq!(emails.len(), 1);
        assert_eq!(emails[0].to, ["dev@example.com"]);
        assert_eq!(emails[0].subject, "[AI fix] Crash");
        let body = &emails[0].body;
        assert!(body.contains("<strong>Given</strong> a deal without a customer<br>"));
        assert!(body.contains("<strong>Then</strong> GET /v1/deals/7 answers 500</p>"));
        assert!(body.contains("<strong>Expected:</strong> the deal opens<br>"));
        assert!(body.contains("<strong>Fix:</strong> Guard it<br>"));
        assert!(body.contains("<strong>Test:</strong> Repeat Given/When: it opens.</p>"));
        assert!(emails[0].body.contains(PR_URL));
        assert!(!emails[0].body.contains("<b>foo</b>"));
    }
    assert_eq!(call().await.json::<Value>(), json!({ "changed": false }));
    assert_eq!(sent.lock().unwrap().len(), 1);
}

#[test_db::test]
async fn a_usage_limit_requeues_with_a_pause_and_no_email(pool: MySqlPool) {
    let id = insert_run(&pool, "running", 1).await;
    let (server, sent) = server(&pool).await;
    server
        .post(&url(id, "result"))
        .add_header("Authorization", AUTH)
        .json(&bare("usage_limited", "limit"))
        .await
        .assert_status_ok();
    let now = row(&pool, id).await;
    assert_eq!(now["status"], "queued");
    assert_eq!(now["last_error"], "usage limit");
    assert_eq!(now["started"], false);
    assert!(now["retry_at"].is_string());
    let minutes: i64 = sqlx::query_scalar(
        "SELECT TIMESTAMPDIFF(MINUTE, UTC_TIMESTAMP(), retry_at) FROM posthog_autofix_runs WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!((58..=60).contains(&minutes), "retry in {minutes} minutes");
    assert!(sent.lock().unwrap().is_empty());
}

#[test_db::test]
async fn a_usage_limit_without_attempts_left_fails_without_an_email(pool: MySqlPool) {
    let id = insert_run(&pool, "running", 3).await;
    let (server, sent) = server(&pool).await;
    server
        .post(&url(id, "result"))
        .add_header("Authorization", AUTH)
        .json(&bare("usage_limited", "limit"))
        .await
        .assert_status_ok();
    let now = row(&pool, id).await;
    assert_eq!(now["status"], "failed");
    assert_eq!(now["last_error"], "usage limit, out of attempts");
    assert_eq!(now["notified"], false);
    assert!(sent.lock().unwrap().is_empty());
}

#[test_db::test]
async fn a_late_result_after_a_timeout_is_recorded_and_emailed(pool: MySqlPool) {
    let id = insert_run(&pool, "timed_out", 1).await;
    let (server, sent) = server(&pool).await;
    server
        .post(&url(id, "result"))
        .add_header("Authorization", AUTH)
        .json(&completed())
        .await
        .assert_status_ok();
    assert_eq!(row(&pool, id).await["status"], "succeeded");
    assert_eq!(sent.lock().unwrap().len(), 1);
}

#[test_db::test]
async fn a_failed_result_records_the_notes_and_sends_no_email(pool: MySqlPool) {
    let id = insert_run(&pool, "running", 1).await;
    let (server, sent) = server(&pool).await;
    server
        .post(&url(id, "result"))
        .add_header("Authorization", AUTH)
        .json(&bare("failed", "push rejected"))
        .await
        .assert_status_ok();
    let now = row(&pool, id).await;
    assert_eq!(now["status"], "failed");
    assert_eq!(now["last_error"], "push rejected");
    assert_eq!(now["finished"], true);
    assert_eq!(now["notified"], false);
    assert!(sent.lock().unwrap().is_empty());
}

#[test_db::test]
async fn a_result_without_a_pull_request_is_recorded_and_sends_no_email(pool: MySqlPool) {
    for outcome in [
        "not_a_code_bug",
        "duplicate",
        "needs_human",
        "not_reproducible",
    ] {
        let id = insert_run(&pool, "running", 1).await;
        let (server, sent) = server(&pool).await;
        let mut body = completed();
        body["agent"]["outcome"] = json!(outcome);
        body["agent"]["test"] = json!({ "status": "not_applicable" });
        body["pr_url"] = Value::Null;
        body["verification"] = json!("not_applicable");
        server
            .post(&url(id, "result"))
            .add_header("Authorization", AUTH)
            .json(&body)
            .await
            .assert_status_ok();
        let now = row(&pool, id).await;
        assert_eq!(now["status"], "succeeded", "{outcome}");
        assert_eq!(now["outcome"], outcome);
        assert_eq!(now["notified"], false, "{outcome}");
        assert!(sent.lock().unwrap().is_empty(), "{outcome}");
    }
}

#[test_db::test]
async fn a_result_for_a_run_that_never_started_is_a_conflict(pool: MySqlPool) {
    let id = insert_run(&pool, "queued", 0).await;
    let (server, sent) = server(&pool).await;
    server
        .post(&url(id, "result"))
        .add_header("Authorization", AUTH)
        .json(&completed())
        .await
        .assert_status(StatusCode::CONFLICT);
    assert!(sent.lock().unwrap().is_empty());
}

#[test_db::test]
async fn invalid_bodies_are_rejected(pool: MySqlPool) {
    let id = insert_run(&pool, "running", 1).await;
    let (server, sent) = server(&pool).await;
    let post = |body: Value| {
        let request = server
            .post(&url(id, "result"))
            .add_header("Authorization", AUTH);
        async move { request.json(&body).await }
    };
    let mut bad_pr = completed();
    bad_pr["pr_url"] = json!("https://evil.example/pull/1");
    post(bad_pr).await.assert_status(StatusCode::BAD_REQUEST);
    let mut no_agent = completed();
    no_agent["agent"] = Value::Null;
    post(no_agent).await.assert_status(StatusCode::BAD_REQUEST);
    let mut bad_id = completed();
    bad_id["issue_id"] = json!("NOT-A-UUID");
    post(bad_id).await.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(row(&pool, id).await["status"], "running");
    assert!(sent.lock().unwrap().is_empty());
}
