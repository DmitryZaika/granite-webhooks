mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::Value;
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn super_users_see_a_features_usage(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO ai_usage_log (feature, api_kind, model, user_id, company_id, input_tokens, output_tokens, cost_usd, success, duration_ms, created_at) VALUES
           ('chat', 'chat', 'gpt-mini', 100, 100, 100, 50, 0.002, 1, 900, '2026-05-01 10:00:00'),
           ('chat', 'chat', 'gpt-mini', 100, 100, 300, 70, 0.004, 0, 1200, '2026-05-02 10:00:00'),
           ('chat', 'chat', 'gpt-mini', NULL, 101, 10, 10, 0.001, 1, 100, '2026-05-02 11:00:00'),
           ('other', 'chat', 'gpt-mini', 100, 100, 1, 1, 9, 1, 1, '2026-05-02 11:00:00');
         UPDATE users SET is_superuser = 1 WHERE id = 102;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    server
        .get("/v1/ai-usage/feature?feature=chat")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let usage: Value = server
        .get("/v1/ai-usage/feature?feature=chat&company_id=100&call_limit=1")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(usage["metrics"]["requests"], 2);
    assert_eq!(usage["metrics"]["success_rate"], 0.5);
    assert_eq!(usage["metrics"]["avg_input_tokens"], 200);
    assert_eq!(usage["top_users"][0]["name"], "Rita Rep");
    assert_eq!(usage["recent_calls"].as_array().unwrap().len(), 1);
    assert_eq!(usage["recent_calls"][0]["success"], false);
    assert_eq!(usage["has_more_recent_calls"], true);
    let all: Value = server
        .get("/v1/ai-usage/feature?feature=chat&from_date=2026-05-02")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(all["metrics"]["requests"], 2);
    server
        .get("/v1/ai-usage/feature?feature=bad%20key")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}
