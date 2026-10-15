mod common;

use api::app;
use api::state::AppState;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;

async fn server(pool: MySqlPool) -> TestServer {
    let router = Router::new().route(
        "/calls/index.json",
        get(|Query(query): Query<HashMap<String, String>>| async move {
            assert_eq!(query["user_id"], "agent-7");
            assert_eq!(query["pageNumber"], "1");
            Json(json!({ "responseData": { "data": [
                { "Cdr": { "id": "11", "type": "incoming", "talking_time": "125", "recorded": true,
                           "started_at": "2026-05-01 10:00:00", "public_external": "+13175550100" },
                  "Contact": { "name": "Alice Lead" }, "Tags": [{ "name": "Hot" }],
                  "Notes": [{ "name": "Wants quartz" }] },
                { "Cdr": { "id": "12", "type": "incoming", "talking_time": "0", "is_voicemail": true,
                           "started_at": "2026-05-02 10:00:00" } },
                { "Cdr": { "id": "13", "type": "outgoing", "talking_time": "0",
                           "started_at": "2026-04-30 10:00:00" } },
            ] } }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN)
        .with_cloudtalk_base(&format!("http://{address}"));
    TestServer::new(app(state))
}

#[test_db::test(seed)]
async fn an_admin_sees_reps_calls(pool: MySqlPool) {
    sqlx::raw_sql(
        "UPDATE company SET cloudtalk_access_key = 'k', cloudtalk_access_secret = 's' WHERE id = 100;
         UPDATE users SET cloudtalk_agent_id = 'agent-7' WHERE id = 100;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool).await;
    let body: Value = server
        .get("/v1/users/call-activity?user_ids=100,101,106")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let calls = body.as_array().unwrap();
    let ids: Vec<i64> = calls
        .iter()
        .map(|call| call["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [12, 11, 13]);
    assert_eq!(calls[0]["action"], "Missed a call");
    assert_eq!(calls[0]["summary"], "Incoming · Voicemail");
    assert_eq!(calls[1]["action"], "Received a call");
    assert_eq!(calls[1]["summary"], "Incoming · 2m 5s");
    assert_eq!(
        calls[1]["tags"],
        json!(["Incoming", "2m 5s", "Recorded", "Hot"])
    );
    assert_eq!(calls[1]["body"], "Wants quartz");
    assert_eq!(calls[1]["customer_name"], "Alice Lead");
    assert_eq!(calls[1]["user_name"], "Rita Rep");
    assert_eq!(calls[2]["summary"], "Outgoing · No answer");
    server
        .get("/v1/users/call-activity?user_ids=100")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}
