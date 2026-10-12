mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn make_super(pool: &MySqlPool) {
    sqlx::raw_sql(
        "UPDATE users SET is_superuser = 1 WHERE id = 103;
         UPDATE company SET signature_enabled = 1 WHERE id = 100;
         UPDATE company SET signature_enabled = 0 WHERE id = 101;
         INSERT INTO onboarding_feedback (id, user_id, company_id, rating, feedback_text, created_at) VALUES
           (9400, 100, 100, 4, 'Nice tours', '2026-09-01 10:00:00');",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn super_admins_read_onboarding_feedback(pool: MySqlPool) {
    make_super(&pool).await;
    let server = server(pool);
    let rows: Value = server
        .get("/v1/superuser/onboarding-feedbacks")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(rows[0]["rating"], 4);
    assert_eq!(rows[0]["user_name"], "Rita Rep");
    assert_eq!(rows[0]["created_at"], "2026-09-01T10:00:00.000Z");
    server
        .get("/v1/superuser/onboarding-feedbacks")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn super_admins_switch_signature_access(pool: MySqlPool) {
    make_super(&pool).await;
    let server = server(pool.clone());
    let body: Value = server
        .get("/v1/superuser/signature-access?company_id=101")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    // Company 101 has signatures off: not offered, so the filter is dropped.
    assert_eq!(body["company_id"], Value::Null);
    let companies: Vec<i64> = body["companies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|company| company["id"].as_i64().unwrap())
        .collect();
    assert!(companies.contains(&100) && !companies.contains(&101));
    let rita = body["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|user| user["id"] == 100)
        .unwrap();
    assert_eq!(rita["is_signature_available"], false);
    server
        .put("/v1/superuser/users/100/signature-access")
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "enabled": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let on: bool =
        sqlx::query_scalar("SELECT is_signature_available <> 0 FROM users WHERE id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(on);
    server
        .put("/v1/superuser/users/105/signature-access")
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "enabled": true }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put("/v1/superuser/users/100/signature-access")
        .with_session(sessions::ADMIN)
        .json(&json!({ "enabled": false }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn super_admins_read_widget_analytics(pool: MySqlPool) {
    make_super(&pool).await;
    sqlx::query(
        "INSERT INTO widget_analytics_events (user_id, company_id, widget_id, action, target) VALUES
           (100, 100, 'leaderboard', 'click', 'row'), (100, 100, 'leaderboard', 'click', 'row'),
           (100, 100, 'leaderboard', 'add', 'dashboard')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO user_dashboard_widgets (user_id, company_id, widget_id, position)
         VALUES (100, 100, 'leaderboard', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let body: Value = server
        .get("/v1/superuser/widget-analytics")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(body["top_used"][0]["uses"], 2);
    assert_eq!(body["events"].as_array().unwrap().len(), 3);
    assert_eq!(body["current_widgets"][0]["widget_id"], "leaderboard");
    server
        .get("/v1/superuser/widget-analytics")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN);
}
