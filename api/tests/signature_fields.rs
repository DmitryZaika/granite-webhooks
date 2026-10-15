mod common;

use api::ai::Ai;
use api::app;
use api::state::AppState;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

async fn server(
    pool: &MySqlPool,
    reply: Option<&'static str>,
) -> (TestServer, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::clone(&seen);
    let router = Router::new().route(
        "/chat",
        post(move |Json(body): Json<Value>| {
            let calls = Arc::clone(&calls);
            async move {
                calls.lock().unwrap().push(body);
                match reply {
                    Some(content) => (
                        StatusCode::OK,
                        Json(json!({
                            "choices": [{ "message": { "content": content } }],
                            "usage": { "prompt_tokens": 10, "completion_tokens": 5 },
                        })),
                    ),
                    None => (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "error": "model_not_found" })),
                    ),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_ai(Ai::new(Some("key"), &format!("http://{address}/chat")));
    (TestServer::new(app(state)), seen)
}

fn candidate(id: i64, before: &str) -> Value {
    json!({ "id": id, "page": 1, "x": 100, "y": 100 + id * 40, "width": 200, "height": 14,
            "before": before, "after": "", "previousLine": "" })
}

#[test_db::test(seed)]
async fn blanks_are_labeled_by_the_ai(pool: MySqlPool) {
    let (server, seen) = server(
        &pool,
        Some(r#"{"fields":[{"id":0,"type":"text","label":"Customer name"},{"id":1,"type":"signature","label":"Signature"},{"id":99,"type":"text","label":"Invented"}]}"#),
    )
    .await;
    let body = server
        .post("/v1/ai/signature-fields")
        .with_session(sessions::REP)
        .json(
            &json!({ "candidates": [candidate(0, "Customer name:"), candidate(1, "Signature:")] }),
        )
        .await
        .json::<Value>();
    assert_eq!(
        body,
        json!({ "fields": [
            { "id": 0, "type": "text", "label": "Customer name" },
            { "id": 1, "type": "signature", "label": "Signature" },
        ] })
    );
    let sent = seen.lock().unwrap().clone();
    assert_eq!(sent[0]["model"], "gpt-6-luna");
    assert_eq!(sent[0]["reasoning_effort"], "none");
    assert_eq!(sent[0]["response_format"], json!({ "type": "json_object" }));
    let user: Value =
        serde_json::from_str(sent[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        user["blanks"][0],
        json!({ "id": 0, "page": 1, "before": "Customer name:", "after": "", "lineAbove": "" })
    );
    let feature: String =
        sqlx::query_scalar("SELECT feature FROM ai_usage_log ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(feature, "signature_fields");
}

#[test_db::test(seed)]
async fn the_ai_failing_is_a_503_for_the_rules_fallback(pool: MySqlPool) {
    for reply in [None, Some("not json")] {
        let (server, _) = server(&pool, reply).await;
        let response = server
            .post("/v1/ai/signature-fields")
            .with_session(sessions::REP)
            .json(&json!({ "candidates": [candidate(0, "Title:")] }))
            .await;
        response.assert_status(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.json::<Value>(),
            json!({ "error": "AI detection is unavailable" })
        );
    }
}

#[test_db::test(seed)]
async fn malformed_or_too_many_blanks_are_refused(pool: MySqlPool) {
    let (server, seen) = server(&pool, Some("{}")).await;
    let empty = server
        .post("/v1/ai/signature-fields")
        .with_session(sessions::REP)
        .json(&json!({ "candidates": [] }))
        .await
        .json::<Value>();
    assert_eq!(empty, json!({ "fields": [] }));
    let many: Vec<Value> = (0..301).map(|id| candidate(id, "x")).collect();
    let mut bad_page = candidate(0, "x");
    bad_page["page"] = json!(0);
    let mut long = candidate(0, "x");
    long["before"] = json!("y".repeat(201));
    for candidates in [json!(many), json!([bad_page]), json!([long])] {
        server
            .post("/v1/ai/signature-fields")
            .with_session(sessions::REP)
            .json(&json!({ "candidates": candidates }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    assert!(seen.lock().unwrap().is_empty());
    server
        .post("/v1/ai/signature-fields")
        .json(&json!({ "candidates": [] }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}
