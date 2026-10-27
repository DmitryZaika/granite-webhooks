//! `POST /v1/ai/first-contact-email` against an `OpenAI` stand-in.

use crate::common;

use api::ai::Ai;
use api::app;
use api::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

type Requests = Arc<Mutex<Vec<Value>>>;

async fn complete(State(requests): State<Requests>, Json(request): Json<Value>) -> Json<Value> {
    requests.lock().unwrap().push(request);
    Json(json!({
        "model": "gpt-6-luna",
        "choices": [{ "message": { "content": "Your kitchen quote\n---BODY---\nHi Alice, this is Rita with Acme Stone. When could we come measure?" }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 500, "completion_tokens": 40 }
    }))
}

async fn setup(pool: &MySqlPool) -> (TestServer, Requests) {
    sqlx::query("UPDATE customers SET details = 'Can you quote my kitchen island?', referral_source = 'Facebook' WHERE id = 1000")
        .execute(pool)
        .await
        .unwrap();
    let requests = Requests::default();
    let router = Router::new()
        .route("/v1/chat/completions", post(complete))
        .with_state(Arc::clone(&requests));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN).with_ai(Ai::new(
        Some("sk-test"),
        &format!("{base}/v1/chat/completions"),
    ));
    (TestServer::new(app(state)), requests)
}

fn prompt(requests: &Requests, index: usize) -> String {
    requests.lock().unwrap()[index]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test_db::test(seed)]
async fn a_first_email_is_drafted_from_the_lead(pool: MySqlPool) {
    let (server, requests) = setup(&pool).await;
    let res = server
        .post("/v1/ai/first-contact-email")
        .with_session(sessions::REP)
        .json(&json!({ "deal_id": 8000, "customer_id": 1000 }))
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({ "subject": "Your kitchen quote",
                "body": "Hi Alice, this is Rita with Acme Stone. When could we come measure?" })
    );
    let prompt = prompt(&requests, 0);
    assert!(prompt.contains("- Recipient Name: Alice Lead"), "{prompt}");
    assert!(prompt.contains("- Customer's Original Request: \"Can you quote my kitchen island?\""));
    assert!(prompt.contains("- Source: Facebook"));
    assert!(prompt.contains("Name: Rita Rep, Company: Acme Stone"));
    let logged: String =
        sqlx::query_scalar("SELECT feature FROM ai_usage_log ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(logged, "email_draft");
}

#[test_db::test(seed)]
async fn another_companys_lead_is_not_read(pool: MySqlPool) {
    let (server, requests) = setup(&pool).await;
    // Deal 8000 and customer 1000 are Acme's.
    let res = server
        .post("/v1/ai/first-contact-email")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "deal_id": 8000, "customer_id": 1000 }))
        .await;
    res.assert_status_ok();
    let prompt = prompt(&requests, 0);
    assert!(!prompt.contains("Alice"), "{prompt}");
    assert!(!prompt.contains("kitchen island"));
    server
        .post("/v1/ai/first-contact-email")
        .json(&json!({ "deal_id": 8000 }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}
