//! `POST /v1/ai/conversation-reply`: the AI Reply button's draft, from the
//! conversation the follow-up writer reads (seed plus
//! `fixtures/followups/gather.sql`).

use crate::common;

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
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const FIXTURE: &str = include_str!("../fixtures/followups/gather.sql");

type Replies = Arc<Mutex<VecDeque<(u16, Value)>>>;

/// An OpenAI stand-in answering the scripted replies in order.
async fn server(
    pool: &MySqlPool,
    replies: Vec<(u16, Value)>,
) -> (TestServer, Arc<Mutex<Vec<Value>>>) {
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let replies: Replies = Arc::new(Mutex::new(replies.into()));
    let calls = Arc::clone(&seen);
    let router = Router::new().route(
        "/chat",
        post(move |Json(body): Json<Value>| {
            let calls = Arc::clone(&calls);
            let replies = Arc::clone(&replies);
            async move {
                calls.lock().unwrap().push(body);
                let (status, content) = replies.lock().unwrap().pop_front().unwrap();
                if status != 200 {
                    return (StatusCode::from_u16(status).unwrap(), Json(content));
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "choices": [{ "message": { "content": content.to_string() } }],
                        "usage": { "prompt_tokens": 900, "completion_tokens": 30 },
                    })),
                )
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

#[test_db::test(seed)]
async fn a_fast_reply_reads_the_whole_conversation(pool: MySqlPool) {
    let (server, seen) = server(
        &pool,
        vec![(
            200,
            json!({ "body": "  Sure thing,\n\nI will send the price today!  " }),
        )],
    )
    .await;
    let res = server
        .post("/v1/ai/conversation-reply")
        .with_session(sessions::REP)
        .json(&json!({ "channel": "sms", "customer_id": 1000 }))
        .await;
    res.assert_status_ok();
    // SMS is one line.
    assert_eq!(
        res.json::<Value>(),
        json!({ "body": "Sure thing, I will send the price today!" })
    );
    let sent = seen.lock().unwrap().clone();
    // The larger model drafts.
    assert_eq!(sent[0]["model"], "gpt-6.1-sol");
    assert_eq!(sent[0]["reasoning_effort"], "low");
    assert_eq!(sent[0]["response_format"], json!({ "type": "json_object" }));
    let user = sent[0]["messages"][1]["content"].as_str().unwrap();
    assert!(
        user.starts_with(
            "Draft the next SMS message for sales rep Rita Rep to send to this customer."
        ),
        "{user}"
    );
    assert!(user.contains("REPLY MODE: Fast reply"));
    assert!(user.contains("[2026-09-20 15:00] SMS Customer: Hi, we are still thinking about the white quartz. Can you send the price?"));
    assert!(user.contains("[2026-09-10 10:00] EMAIL Customer: Subject: Kitchen quote"));
    assert!(user.contains("COMPANY ADMINISTRATIVE INSTRUCTIONS / GUARDRAILS:\nTone\nBe friendly."));
    assert!(user.contains("GREETING: ") && user.contains("SIGNATURE: none"));
    assert!(user.contains("QUOTES"));
    assert!(
        sent[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("You help a countertop sales rep draft")
    );
    let logged: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM ai_usage_log WHERE feature = 'quick_reply'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(logged, 1);
}

#[test_db::test(seed)]
async fn an_email_draft_drops_a_subject_line_and_falls_back_to_the_reply_model(pool: MySqlPool) {
    let (server, seen) = server(
        &pool,
        vec![
            (503, json!({ "error": { "message": "The server is overloaded" } })),
            (403, json!({ "error": { "message": "The project does not have access to model gpt-6-luna" } })),
            (200, json!({ "body": "Subject: Your quote\n\nHi Alice, here is the price." })),
        ],
    )
    .await;
    let res = server
        .post("/v1/ai/conversation-reply")
        .with_session(sessions::REP)
        .json(&json!({ "channel": "email", "customer_id": 1000, "mode": "custom", "custom_request": "Send the price",
                        "include_instructions": false }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["body"], "Hi Alice, here is the price.");
    let sent = seen.lock().unwrap().clone();
    // The large model failed, the quick one has no access, its fallback drafts.
    assert_eq!(sent[0]["model"], "gpt-6.1-sol");
    assert_eq!(sent[1]["model"], "gpt-6-luna");
    assert!(sent[1].get("reasoning_effort").is_none());
    assert_eq!(sent[2]["model"], "gpt-5.6-luna");
    let user = sent[2]["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains("REPLY MODE: Custom.\nThe sales rep gave this instruction for the draft:\n\"Send the price\""));
    assert!(user.contains("COMPANY ADMINISTRATIVE INSTRUCTIONS / GUARDRAILS:\n(none)"));
}

#[test_db::test(seed)]
async fn an_empty_draft_is_an_error(pool: MySqlPool) {
    // The large model and the quick one both write nothing.
    let (server, seen) = server(
        &pool,
        vec![
            (200, json!({ "body": "   " })),
            (200, json!({ "body": "" })),
        ],
    )
    .await;
    let res = server
        .post("/v1/ai/conversation-reply")
        .with_session(sessions::REP)
        .json(&json!({ "channel": "sms", "phone_digits": "3175550100" }))
        .await;
    res.assert_status(StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(res.json::<Value>()["error"], "Failed to generate AI reply");
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[test_db::test(seed)]
async fn requests_are_checked(pool: MySqlPool) {
    let (server, seen) = server(&pool, Vec::new()).await;
    for (body, status, error) in [
        (
            json!({ "channel": "sms" }),
            StatusCode::BAD_REQUEST,
            "Customer or phone required",
        ),
        (
            json!({ "channel": "sms", "phone_digits": "12ab" }),
            StatusCode::BAD_REQUEST,
            "Invalid phone number",
        ),
        (
            json!({ "channel": "fax", "customer_id": 1000 }),
            StatusCode::BAD_REQUEST,
            "Invalid request data",
        ),
        (
            json!({ "channel": "sms", "customer_id": 1000, "mode": "custom", "custom_request": "  " }),
            StatusCode::BAD_REQUEST,
            "Custom request required",
        ),
        (
            json!({ "channel": "email", "customer_email": "no-at-sign" }),
            StatusCode::BAD_REQUEST,
            "Customer or phone required",
        ),
    ] {
        let res = server
            .post("/v1/ai/conversation-reply")
            .with_session(sessions::REP)
            .json(&body)
            .await;
        res.assert_status(status);
        assert_eq!(res.json::<Value>()["error"], error, "{body}");
    }
    server
        .post("/v1/ai/conversation-reply")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "channel": "sms", "customer_id": 1000 }))
        .await
        .assert_status_not_found();
    server
        .post("/v1/ai/conversation-reply")
        .json(&json!({ "channel": "sms", "customer_id": 1000 }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert!(seen.lock().unwrap().is_empty());
}
