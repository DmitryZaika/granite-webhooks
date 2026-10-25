use crate::common;

use api::app;
use api::state::AppState;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// Company 101 (Olga, 106) has CloudTalk keys; Acme (100) has none.

async fn server(pool: MySqlPool) -> (TestServer, Arc<Mutex<Vec<Value>>>) {
    let started = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::clone(&started);
    let router = Router::new()
        .route(
            "/calls/index.json",
            get(|Query(query): Query<HashMap<String, String>>| async move {
                let phone = query["public_external"].clone();
                assert_eq!(query["date_from"], "2026-04-01");
                Json(json!({ "responseData": { "data": [
                    { "Cdr": { "id": "1", "public_external": phone } },
                    { "Cdr": { "id": "shared", "public_external": phone } },
                ] } }))
            }),
        )
        .route(
            "/calls/create.json",
            post(|Json(body): Json<Value>| async move {
                if body["callee_number"] == "+13175550000" {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "responseData": { "message": "Agent is not online" } })),
                    );
                }
                calls.lock().unwrap().push(body);
                (
                    StatusCode::OK,
                    Json(json!({ "responseData": { "status": 200 } })),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN)
        .with_cloudtalk_base(&format!("http://{address}"));
    (TestServer::new(app(state)), started)
}

#[test_db::test(seed)]
async fn cloudtalk_calls_are_listed_once(pool: MySqlPool) {
    let (server, _) = server(pool).await;
    let body: Value = server
        .get("/v1/calls/cloudtalk?phone=463-555-0200&phone2=4635550201&date_from=2026-04-01")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["Cdr"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["1", "shared"]);
    assert_eq!(body["items"][0]["Cdr"]["public_external"], "+14635550200");
    // No CloudTalk keys: nothing to show.
    let acme: Value = server
        .get("/v1/calls/cloudtalk?phone=317-555-0100")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(acme, json!({ "items": [] }));
}

#[test_db::test(seed)]
async fn a_call_rings_the_agent(pool: MySqlPool) {
    let (server, started) = server(pool.clone()).await;
    let call = |digits: &str| {
        let request = server
            .post("/v1/calls/cloudtalk")
            .with_session(sessions::OTHER_COMPANY_REP)
            .json(&json!({ "phone_digits": digits }));
        async move { request.await }
    };
    let unlinked = call("4635550200").await;
    unlinked.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(unlinked.json::<Value>()["error"], "agent_not_linked");
    sqlx::query("UPDATE users SET cloudtalk_agent_id = '7001' WHERE id = 106")
        .execute(&pool)
        .await
        .unwrap();
    call("4635550200").await.assert_status_ok();
    assert_eq!(
        started.lock().unwrap().clone(),
        [json!({ "agent_id": 7001, "callee_number": "+14635550200" })]
    );
    let offline = call("3175550000").await;
    offline.assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(offline.json::<Value>()["error"], "agent_not_online");
    call("12-34").await.assert_status(StatusCode::BAD_REQUEST);
    let acme = server
        .post("/v1/calls/cloudtalk")
        .with_session(sessions::REP)
        .json(&json!({ "phone_digits": "3175550100" }))
        .await;
    assert_eq!(acme.json::<Value>()["error"], "cloudtalk_not_configured");
}
