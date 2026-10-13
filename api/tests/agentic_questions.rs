//! `POST /v1/ai/agentic-questions` and `GET /v1/ai/agentic-questions/{id}`:
//! questions queued for the claude-job-runner agent.

mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[derive(sqlx::FromRow, Debug)]
struct Queued {
    chat_id: i64,
    content: String,
    is_agentic: Option<i8>,
    admin: Option<String>,
}

async fn queued(pool: &MySqlPool, id: i64) -> Queued {
    sqlx::query_as(
        "SELECT CAST(chat_id AS SIGNED) AS chat_id, content, is_agentic,
                JSON_TYPE(JSON_EXTRACT(payload, '$.requester_is_admin')) AS admin
           FROM chat_messages WHERE id = ? AND sender = 'user'",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn chat(pool: &MySqlPool, id: i64) -> (i64, i64, String) {
    sqlx::query_as(
        "SELECT CAST(user_id AS SIGNED), CAST(company_id AS SIGNED), title FROM chats WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn a_new_conversation_creates_a_chat_and_queues_the_question(pool: MySqlPool) {
    let server = server(pool.clone());
    let response = server
        .post("/v1/ai/agentic-questions")
        .with_session(sessions::REP)
        .json(&json!({ "query": "  Show\n my deals  ", "new_chat": true }))
        .await;
    response.assert_status(StatusCode::CREATED);
    let body: Value = response.json();
    assert_eq!(body["status"], "queued");
    let row = queued(&pool, body["id"].as_i64().unwrap()).await;
    assert_eq!(row.content, "Show\n my deals");
    assert_eq!(row.is_agentic, Some(1));
    // A real JSON boolean, which the runner reads.
    assert_eq!(row.admin.as_deref(), Some("BOOLEAN"));
    let flag: i64 = sqlx::query_scalar("SELECT JSON_EXTRACT(payload, '$.requester_is_admin') = CAST('false' AS JSON) FROM chat_messages WHERE id = ?")
        .bind(body["id"].as_i64().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(flag, 1);
    assert_eq!(
        chat(&pool, row.chat_id).await,
        (100, 100, "Show my deals".to_string())
    );
}

#[test_db::test(seed)]
async fn admins_are_flagged_so_the_agent_may_show_everyones_data(pool: MySqlPool) {
    let server = server(pool.clone());
    let body: Value = server
        .post("/v1/ai/agentic-questions")
        .with_session(sessions::ADMIN)
        .json(&json!({ "query": "Q", "new_chat": true }))
        .await
        .json();
    let flag: i64 = sqlx::query_scalar("SELECT JSON_EXTRACT(payload, '$.requester_is_admin') = CAST('true' AS JSON) FROM chat_messages WHERE id = ?")
        .bind(body["id"].as_i64().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(flag, 1);
}

#[test_db::test(seed)]
async fn a_follow_up_reuses_the_latest_chat_and_a_first_one_creates_it(pool: MySqlPool) {
    let server = server(pool.clone());
    // No chat yet: a follow-up creates one.
    let first: Value = server
        .post("/v1/ai/agentic-questions")
        .with_session(sessions::REP)
        .json(&json!({ "query": "First" }))
        .await
        .json();
    let first_chat = queued(&pool, first["id"].as_i64().unwrap()).await.chat_id;
    sqlx::query("UPDATE chats SET updated_at = '2020-01-01' WHERE id = ?")
        .bind(first_chat)
        .execute(&pool)
        .await
        .unwrap();
    let second: Value = server
        .post("/v1/ai/agentic-questions")
        .with_session(sessions::REP)
        .json(&json!({ "query": "And last month?", "new_chat": false }))
        .await
        .json();
    assert_eq!(
        queued(&pool, second["id"].as_i64().unwrap()).await.chat_id,
        first_chat
    );
    let touched: i64 =
        sqlx::query_scalar("SELECT updated_at > '2020-01-02' FROM chats WHERE id = ?")
            .bind(first_chat)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(touched, 1);
    // Another user's chat is never reused.
    let other: Value = server
        .post("/v1/ai/agentic-questions")
        .with_session(sessions::MANAGER)
        .json(&json!({ "query": "Mine" }))
        .await
        .json();
    assert_ne!(
        queued(&pool, other["id"].as_i64().unwrap()).await.chat_id,
        first_chat
    );
}

#[test_db::test(seed)]
async fn empty_or_too_long_questions_and_anonymous_callers_are_refused(pool: MySqlPool) {
    let server = server(pool);
    for query in ["   ".to_string(), "é".repeat(8001)] {
        server
            .post("/v1/ai/agentic-questions")
            .with_session(sessions::REP)
            .json(&json!({ "query": query }))
            .await
            .assert_status_bad_request();
    }
    server
        .post("/v1/ai/agentic-questions")
        .with_session(sessions::REP)
        .json(&json!({ "query": "é".repeat(8000) }))
        .await
        .assert_status(StatusCode::CREATED);
    server
        .post("/v1/ai/agentic-questions")
        .json(&json!({ "query": "Q" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .get("/v1/ai/agentic-questions/1")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn the_asker_follows_the_question_to_its_answer(pool: MySqlPool) {
    let server = server(pool.clone());
    let body: Value = server
        .post("/v1/ai/agentic-questions")
        .with_session(sessions::REP)
        .json(&json!({ "query": "How many deals?", "new_chat": true }))
        .await
        .json();
    let id = body["id"].as_i64().unwrap();
    let get = |session: &'static str| {
        server
            .get(&format!("/v1/ai/agentic-questions/{id}"))
            .with_session(session)
    };
    let response = get(sessions::REP).await;
    assert_eq!(response.header("cache-control"), "no-store");
    assert_eq!(
        response.json::<Value>(),
        json!({ "id": id, "status": "queued", "reply": null, "error": null, "partial": null })
    );
    // The runner picks it up, then answers in the same chat.
    let chat_id = queued(&pool, id).await.chat_id;
    sqlx::query("UPDATE chat_messages SET status = 'pending' WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        get(sessions::REP).await.json::<Value>()["status"],
        "pending"
    );
    let reply = sqlx::query("INSERT INTO chat_messages (chat_id, sender, content) VALUES (?, 'ai', 'You have 3 deals.')")
        .bind(chat_id)
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_id();
    sqlx::query("UPDATE chat_messages SET status = 'done', payload = JSON_SET(payload, '$.reply_id', ?) WHERE id = ?")
        .bind(reply)
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        get(sessions::REP).await.json::<Value>(),
        json!({ "id": id, "status": "done", "reply": "You have 3 deals.", "error": null, "partial": null })
    );
    // Someone else's question reads as not found.
    get(sessions::MANAGER).await.assert_status_not_found();
    get(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn a_failed_question_carries_its_error_and_partial_answer(pool: MySqlPool) {
    let server = server(pool.clone());
    let body: Value = server
        .post("/v1/ai/agentic-questions")
        .with_session(sessions::REP)
        .json(&json!({ "query": "Q", "new_chat": true }))
        .await
        .json();
    let id = body["id"].as_i64().unwrap();
    sqlx::query("UPDATE chat_messages SET status = 'failed', payload = JSON_SET(payload, '$.error', 'timeout', '$.result', 'Half') WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let job: Value = server
        .get(&format!("/v1/ai/agentic-questions/{id}"))
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        job,
        json!({ "id": id, "status": "failed", "reply": null, "error": "timeout", "partial": "Half" })
    );
}
