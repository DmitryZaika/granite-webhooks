//! `POST /v1/ai/crm-manual-feedback`: corrections of CRM manual answers and
//! the feedback saved for the super admins.

mod common;

use api::ai::Ai;
use api::app;
use api::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct OpenAi {
    reply: Arc<Mutex<Option<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn complete(State(ai): State<OpenAi>, Json(request): Json<Value>) -> Response {
    ai.requests.lock().unwrap().push(request);
    match ai.reply.lock().unwrap().clone() {
        Some(text) => Json(json!({
            "choices": [{ "message": { "content": text }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5 },
        }))
        .into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "down").into_response(),
    }
}

async fn setup(pool: &MySqlPool, reply: Option<&str>) -> (TestServer, OpenAi) {
    let ai = OpenAi::default();
    *ai.reply.lock().unwrap() = reply.map(str::to_string);
    let router = Router::new()
        .route("/chat", routing::post(complete))
        .with_state(ai.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_ai(Ai::new(Some("sk"), &format!("{base}/chat")));
    (TestServer::new(app(state)), ai)
}

async fn post(server: &TestServer, body: Value) -> axum_test::TestResponse {
    server
        .post("/v1/ai/crm-manual-feedback")
        .with_session(sessions::REP)
        .json(&body)
        .await
}

#[test_db::test(seed)]
async fn a_disliked_answer_gets_the_models_correction(pool: MySqlPool) {
    let (server, ai) = setup(
        &pool,
        Some("Open **CRM > SMS** and click **Mark as unread**.\n[[MANUAL_SECTION:employee-sms]]\n[[MANUAL_PATCH]]\n- Hover the row, click the envelope"),
    )
    .await;
    let res = post(
        &server,
        json!({ "intent": "dislike", "question": "how do I mark an sms as unread", "originalAnswer": "You cannot.", "attempt": 2,
                "previousCorrections": ["Try the inbox."] }),
    )
    .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({ "correction": "Open **CRM > SMS** and click **Mark as unread**.", "attempt": 2,
                "sectionId": "employee-sms", "proposedManualChange": "- Hover the row, click the envelope", "escalate": false })
    );
    let request = ai.requests.lock().unwrap()[0].clone();
    let system = request["messages"][0]["content"].as_str().unwrap();
    assert!(system.starts_with("CURRENT USER ACCESS"), "{system}");
    assert!(system.contains("Positions at current company: Sales Rep"));
    assert!(
        system.contains("CRM USER MANUAL:")
            && system.contains("You are correcting a CRM manual answer")
    );
    let user = request["messages"][1]["content"].as_str().unwrap();
    assert!(
        user.starts_with("Previous correction attempts the user also disliked:\n1. Try the inbox.")
    );
    assert!(user.contains("This is correction attempt 2 of 2."));
    let logged: String =
        sqlx::query_scalar("SELECT feature FROM ai_usage_log ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(logged, "crm_manual_correction");
}

#[test_db::test(seed)]
async fn canned_answers_and_failures_need_no_model(pool: MySqlPool) {
    let (server, ai) = setup(&pool, None).await;
    let res = post(
        &server,
        json!({ "intent": "dislike", "question": "where do I find the cloudtalk phone number", "originalAnswer": "x", "attempt": 1 }),
    )
    .await;
    let body = res.json::<Value>();
    assert!(
        body["correction"]
            .as_str()
            .unwrap()
            .starts_with("The phone number itself lives in CloudTalk")
    );
    assert_eq!(body["sectionId"], "admin-company-settings-users");
    assert!(ai.requests.lock().unwrap().is_empty());

    // Rita is no platform superuser: adding a company is not for her.
    let res = post(&server, json!({ "intent": "dislike", "question": "how do I add a company", "originalAnswer": "x", "attempt": 1 })).await;
    let body = res.json::<Value>();
    assert!(
        body["correction"]
            .as_str()
            .unwrap()
            .starts_with("Creating a platform company requires **platform superuser** access.")
    );

    // The model is down: an apology instead.
    let res = post(&server, json!({ "intent": "dislike", "question": "how do I pin a note", "originalAnswer": "x", "attempt": 1 })).await;
    assert!(
        res.json::<Value>()["correction"]
            .as_str()
            .unwrap()
            .starts_with("I could not generate a better answer right now.")
    );
}

#[test_db::test(seed)]
async fn approved_and_escalated_feedback_is_saved_for_review(pool: MySqlPool) {
    let (server, _) = setup(&pool, None).await;
    let dialog = json!([
        { "role": "user", "content": "how do I mark an sms as unread" },
        { "role": "assistant", "content": "You cannot." },
        { "role": "assistant", "content": "Hover and click the envelope.", "correctionAttempt": 1, "correctionLiked": true },
    ]);
    let res = post(
        &server,
        json!({ "intent": "approve", "dialog": dialog, "correctedAnswer": "Hover and click the envelope.", "attempt": 1, "feedbackText": "" }),
    )
    .await;
    res.assert_status_ok();
    let body = res.json::<Value>();
    assert_eq!(body["message"], "Thanks for confirming the updated answer.");
    let (status, section, history, company): (String, Option<String>, String, Option<i32>) = sqlx::query_as(
        "SELECT status, manual_section_id, CAST(history AS CHAR), company_id FROM message_feedback WHERE id = ?",
    )
    .bind(body["feedbackId"].as_i64().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), section.as_deref(), company),
        ("user_approved", Some("user-positions-access"), Some(100))
    );
    let history: Value = serde_json::from_str(&history).unwrap();
    assert_eq!(history["approvedAnswer"], "Hover and click the envelope.");
    assert_eq!(history["userName"], "Rita Rep");
    assert_eq!(history["dialog"][2]["correctionLiked"], true);
    assert_eq!(history["feedbackText"], Value::Null);

    let res = post(
        &server,
        json!({ "intent": "escalate", "dialog": dialog, "sectionId": "employee-sms", "proposedManualChange": "- step", "feedbackText": "still wrong" }),
    )
    .await;
    assert_eq!(
        res.json::<Value>()["message"],
        "I'm sorry — your review was added to our team and we will check it."
    );
    let (status, proposed, text): (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, proposed_manual_change, feedback_text FROM message_feedback ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), proposed.as_deref(), text.as_deref()),
        ("escalated", Some("- step"), Some("still wrong"))
    );
}

#[test_db::test(seed)]
async fn bad_feedback_is_refused(pool: MySqlPool) {
    let (server, _) = setup(&pool, None).await;
    post(
        &server,
        json!({ "intent": "dislike", "question": "q", "originalAnswer": "a", "attempt": 3 }),
    )
    .await
    .assert_status(StatusCode::BAD_REQUEST);
    let res = post(&server, json!({ "intent": "escalate", "dialog": [] })).await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "Invalid dialog payload");
    post(&server, json!({ "intent": "vote" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/ai/crm-manual-feedback")
        .with_session(sessions::VIEWER)
        .json(&json!({ "intent": "escalate", "dialog": [{ "role": "user", "content": "x" }] }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}
