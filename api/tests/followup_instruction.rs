mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const TITLE: &str = "AI follow-up instruction";

async fn rows(pool: &MySqlPool, company_id: i32) -> Vec<(i32, Option<String>)> {
    sqlx::query_as(
        "SELECT id, rich_text FROM instructions WHERE company_id = ? AND title = ? ORDER BY id",
    )
    .bind(company_id)
    .bind(TITLE)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn empty_when_the_company_has_none(pool: MySqlPool) {
    let server = server(pool);
    let body = server
        .get("/v1/followups/instruction")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(body, json!({ "instruction": "" }));
}

#[test_db::test(seed)]
async fn reads_the_first_row_as_plain_text_and_scopes_by_company(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO instructions (id, title, rich_text, company_id) VALUES
           (9400, 'AI follow-up instruction', '  <p>Two <b>days</b> between</p> ', 100),
           (9401, 'AI follow-up instruction', 'second copy', 100),
           (9402, 'AI follow-up instruction', 'other company', 101),
           (9403, 'Something else', 'not it', 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let mine = server
        .get("/v1/followups/instruction")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(mine, json!({ "instruction": "Two days between" }));
    let theirs = server
        .get("/v1/followups/instruction")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json::<Value>();
    assert_eq!(theirs, json!({ "instruction": "other company" }));
}

#[test_db::test(seed)]
async fn saving_inserts_then_updates_and_drops_duplicates(pool: MySqlPool) {
    let server = server(pool.clone());
    let saved = server
        .put("/v1/followups/instruction")
        .with_session(sessions::REP)
        .json(&json!({ "instruction": "  Every 3 days  " }))
        .await
        .json::<Value>();
    assert_eq!(saved, json!({ "instruction": "Every 3 days" }));
    let first = rows(&pool, 100).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].1.as_deref(), Some("Every 3 days"));

    sqlx::query("INSERT INTO instructions (title, rich_text, company_id) VALUES (?, 'dup', 100)")
        .bind(TITLE)
        .execute(&pool)
        .await
        .unwrap();
    server
        .put("/v1/followups/instruction")
        .with_session(sessions::MANAGER)
        .json(&json!({ "instruction": "Daily" }))
        .await
        .assert_status_ok();
    let after = rows(&pool, 100).await;
    assert_eq!(after, vec![(first[0].0, Some("Daily".to_string()))]);
    let got = server
        .get("/v1/followups/instruction")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(got, json!({ "instruction": "Daily" }));
    // Company 101 is untouched.
    assert!(rows(&pool, 101).await.is_empty());
}

#[test_db::test(seed)]
async fn a_blank_instruction_deletes_every_copy(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO instructions (title, rich_text, company_id) VALUES
           ('AI follow-up instruction', 'a', 100), ('AI follow-up instruction', 'b', 100),
           ('AI follow-up instruction', 'keep', 101)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());
    let saved = server
        .put("/v1/followups/instruction")
        .with_session(sessions::REP)
        .json(&json!({ "instruction": "   " }))
        .await
        .json::<Value>();
    assert_eq!(saved, json!({ "instruction": "" }));
    assert!(rows(&pool, 100).await.is_empty());
    assert_eq!(rows(&pool, 101).await.len(), 1);
}

#[test_db::test(seed)]
async fn too_long_is_refused_without_saving(pool: MySqlPool) {
    let server = server(pool.clone());
    let at_limit = "é".repeat(2000);
    server
        .put("/v1/followups/instruction")
        .with_session(sessions::REP)
        .json(&json!({ "instruction": format!(" {at_limit} ") }))
        .await
        .assert_status_ok();
    let response = server
        .put("/v1/followups/instruction")
        .with_session(sessions::REP)
        .json(&json!({ "instruction": format!("{at_limit}x") }))
        .await;
    response.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        rows(&pool, 100).await[0].1.as_deref(),
        Some(at_limit.as_str())
    );
    server
        .put("/v1/followups/instruction")
        .with_session(sessions::REP)
        .json(&json!({}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn needs_an_employee_session(pool: MySqlPool) {
    let server = server(pool);
    server
        .get("/v1/followups/instruction")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .put("/v1/followups/instruction")
        .json(&json!({ "instruction": "x" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .get("/v1/followups/instruction")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}
