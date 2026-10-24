//! The AI follow-up of the flow picker: preview, rewrite one message, start
//! or switch to it (`/v1/followups/ai-cadence*`), against an `OpenAI`
//! stand-in.

mod common;

use api::ai::Ai;
use api::app;
use api::state::AppState;
use api::storage::Storage;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// Acme (100) texts through CloudTalk; Rita (100) has a line (agent 5001,
// 317-555-0000) and is the sales rep of Paula (9600, 317-555-0177,
// paula@example.com), whom nobody has written to yet.

const FIXTURE: &str = "
UPDATE company SET cloudtalk_access_key = 'ct-key', cloudtalk_access_secret = 'ct-secret' WHERE id = 100;
UPDATE users SET cloudtalk_agent_id = '5001', cloudtalk_phone_number = '3175550000' WHERE id = 100;
INSERT INTO customers (id, name, phone, company_id, source, sales_rep, created_date)
  VALUES (9600, 'Paula Prospect', '317-555-0177', 100, 'leads', 100, '2026-09-01 08:00:00');
INSERT INTO customers_emails (id, customer_id, email, created_at, created_by)
  VALUES (9601, 9600, 'paula@example.com', '2026-09-01 08:00:00', 100);
UPDATE customers SET email_id = 9601 WHERE id = 9600;
";

const DRAFTS: [&str; 8] = [
    "Hi Paula, this is Rita with Acme Stone. Do you have any questions about your countertops?",
    "Paula, would it help to see a few stone samples in person?",
    "Just checking in, Paula. Is your project timeline still the same?",
    "Paula, can I help narrow down colors for you?",
    "Would a quick call help you plan the next step, Paula?",
    "Paula, are you still comparing options for your project?",
    "Is there anything holding the project back that I can help with?",
    "Paula, should I keep you on my list for updates?",
];

#[derive(Clone, Default)]
struct Model {
    replies: Arc<Mutex<VecDeque<Value>>>,
    prompts: Arc<Mutex<Vec<Value>>>,
}

async fn complete(State(model): State<Model>, Json(request): Json<Value>) -> Json<Value> {
    model.prompts.lock().unwrap().push(request);
    // Without a scripted reply: the whole cadence, eight distinct drafts.
    let content = model
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| {
            json!({ "skip": false, "messages": DRAFTS.iter().map(|body| json!({
            "skip": false, "subject": "Your countertops", "body": body, "delayMinutes": 2880
        })).collect::<Vec<_>>() })
        });
    Json(json!({
        "model": "gpt-6.1-sol-2026",
        "choices": [{ "message": { "content": content.to_string() }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 1500, "completion_tokens": 300 }
    }))
}

async fn setup(pool: &MySqlPool) -> (TestServer, Model) {
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    let model = Model::default();
    let router = Router::new()
        .route("/v1/chat/completions", post(complete))
        .with_state(model.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_storage(Storage::memory())
        .with_cloudtalk_base(&base)
        .with_ai(Ai::new(
            Some("sk-test"),
            &format!("{base}/v1/chat/completions"),
        ));
    (TestServer::new(app(state)), model)
}

async fn send(server: &TestServer, path: &str, body: Value) -> axum_test::TestResponse {
    server
        .post(path)
        .with_session(sessions::REP)
        .json(&body)
        .await
}

#[test_db::test(seed)]
async fn a_preview_is_planned_written_and_scheduled(pool: MySqlPool) {
    let (server, model) = setup(&pool).await;
    let res = send(
        &server,
        "/v1/followups/ai-cadence/preview",
        json!({ "customer_id": 9600 }),
    )
    .await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["channel"], "sms");
    assert_eq!(body["cadence_mode"], "new");
    assert_eq!(body["last_contact_at"], Value::Null);
    let preview = body["preview"].as_array().unwrap();
    assert!(!preview.is_empty());
    assert_eq!(body["remaining_step_count"], preview.len());
    assert_eq!(preview[0]["body"], DRAFTS[0]);
    for message in preview {
        assert_eq!(message["generated_body"], message["body"]);
        assert_eq!(message["skip"], false);
        assert!(message["delay_minutes"].as_i64().unwrap() >= 0);
        assert!(!message["delay_label"].as_str().unwrap().is_empty());
        assert!(!message["goal"].as_str().unwrap().is_empty());
    }
    // The model was told about Paula.
    let prompts = model.prompts.lock().unwrap().clone();
    assert!(!prompts.is_empty());
    let user_prompt = prompts[0]["messages"][1]["content"].as_str().unwrap();
    assert!(user_prompt.contains("Paula"), "{user_prompt}");

    // The thread's phone finds the same customer.
    let res = send(
        &server,
        "/v1/followups/ai-cadence/preview",
        json!({ "phone_digits": "3175550177" }),
    )
    .await;
    res.assert_status_ok();
}

#[test_db::test(seed)]
async fn a_customer_waiting_for_a_reply_is_explained_not_written(pool: MySqlPool) {
    let (server, model) = setup(&pool).await;
    sqlx::query(
        "INSERT INTO cloudtalk_sms (sender, recipient, text, direction, status, company_id, created_date, phone_digits)
         VALUES ('3175550177', '3175550000', 'Can you send me the price for the island?', 'inbound', 'received', 100,
                 UTC_TIMESTAMP() - INTERVAL 1 DAY, '3175550177')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let res = send(
        &server,
        "/v1/followups/ai-cadence/preview",
        json!({ "customer_id": 9600 }),
    )
    .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    let body: Value = res.json();
    assert_eq!(body["error"], "nothing_to_send");
    assert_eq!(body["reason_code"], "CUSTOMER_WAITING_FOR_HUMAN_REPLY");
    assert_eq!(body["can_force_generate"], true);
    assert!(
        body["reason_message"]
            .as_str()
            .unwrap()
            .starts_with("They are waiting for a human reply.")
    );
    assert!(model.prompts.lock().unwrap().is_empty());

    // "Generate anyway".
    let res = send(
        &server,
        "/v1/followups/ai-cadence/preview",
        json!({ "customer_id": 9600, "force_generate": true }),
    )
    .await;
    res.assert_status_ok();
    assert!(
        !res.json::<Value>()["preview"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test_db::test(seed)]
async fn a_preview_needs_someone_to_write_to(pool: MySqlPool) {
    let (server, _) = setup(&pool).await;
    let cases = [
        (json!({}), "missing_target"),
        (json!({ "phone_digits": "12ab" }), "invalid_phone"),
        (json!({ "customer_id": 1 }), "customer_not_found"),
    ];
    for (body, error) in cases {
        let res = send(&server, "/v1/followups/ai-cadence/preview", body).await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], error);
    }
    // Another company's customer is not found either.
    let res = server
        .post("/v1/followups/ai-cadence/preview")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "customer_id": 9600 }))
        .await;
    assert_eq!(res.json::<Value>()["error"], "customer_not_found");
    server
        .post("/v1/followups/ai-cadence/preview")
        .json(&json!({ "customer_id": 9600 }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

fn approved() -> Value {
    json!([
        { "delay_minutes": 1440, "delay_label": "+1 day", "goal": "Check in", "channel": "sms", "skip": false,
          "skip_reason": null, "body": "Hi Paula, it's Rita. Any questions on the quote?", "subject": null,
          "generated_body": DRAFTS[0], "generated_subject": null, "angle": "quote_status", "phase": "FIRST_FOLLOWUP" },
        { "delay_minutes": 2880, "delay_label": "+2 days", "goal": "Offer help", "channel": "email", "skip": false,
          "skip_reason": null, "body": "Happy to help you pick a stone.", "subject": "Your countertops",
          "generated_body": "Happy to help you pick a stone.", "generated_subject": "Your countertops",
          "angle": "design_help", "phase": "EARLY_FOLLOWUP" }
    ])
}

#[test_db::test(seed)]
async fn an_approved_follow_up_starts_as_the_reps_flow(pool: MySqlPool) {
    let (server, _) = setup(&pool).await;
    let res = send(
        &server,
        "/v1/followups/ai-cadence",
        json!({ "intent": "start", "customer_id": 9600, "preview": approved() }),
    )
    .await;
    res.assert_status_ok();
    let flow = &res.json::<Value>()["active_flow"];
    assert_eq!(flow["flow_name"], "Generated with AI");
    assert_eq!(flow["status"], "active");
    assert_eq!(flow["total_steps"], 2);
    assert_eq!(
        flow["next_step_preview"],
        "Hi Paula, it's Rita. Any questions on the quote?"
    );

    let steps: Vec<(String, Option<String>, Option<String>, i32)> = sqlx::query_as(
        "SELECT CAST(s.channel AS CHAR), s.custom_text, s.subject, s.delay_minutes
           FROM sms_flow_steps s JOIN sms_flows f ON f.id = s.flow_id
          WHERE f.name = 'Generated with AI' AND f.user_id = 100 ORDER BY s.position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].0, "sms");
    assert_eq!(
        steps[0].1.as_deref(),
        Some("Hi Paula, it's Rita. Any questions on the quote?")
    );
    assert_eq!(steps[1].0, "email");
    assert_eq!(steps[1].2.as_deref(), Some("Your countertops"));

    // Rita's edit of the first draft is learned from.
    let (generated, final_text, followup): (String, String, Option<i32>) = sqlx::query_as(
        "SELECT generated_text, final_text, followup_id FROM ai_followup_edit_pairs WHERE user_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(generated, DRAFTS[0]);
    assert_eq!(
        final_text,
        "Hi Paula, it's Rita. Any questions on the quote?"
    );
    assert!(followup.is_some());
    let samples: i64 = sqlx::query_scalar(
        "SELECT sample_count FROM ai_followup_style_preferences WHERE user_id = 100 AND company_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(samples, 1);

    // A second start is refused; a switch replaces the running flow.
    let res = send(
        &server,
        "/v1/followups/ai-cadence",
        json!({ "intent": "start", "customer_id": 9600, "preview": approved() }),
    )
    .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(res.json::<Value>()["error"], "flow_already_active");
    let res = send(
        &server,
        "/v1/followups/ai-cadence",
        json!({ "intent": "switch", "customer_id": 9600, "preview": approved() }),
    )
    .await;
    res.assert_status_ok();
    let statuses: Vec<String> = sqlx::query_scalar(
        "SELECT CAST(status AS CHAR) FROM sms_flow_enrollments WHERE customer_id = 9600 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(statuses, ["cancelled", "active"]);
}

#[test_db::test(seed)]
async fn a_start_needs_approved_messages(pool: MySqlPool) {
    let (server, _) = setup(&pool).await;
    for preview in [
        json!([]),
        json!([{ "delay_minutes": -1, "delay_label": "", "goal": "x", "channel": "sms",
                                        "skip": false, "skip_reason": null, "body": "x", "subject": null }]),
    ] {
        let res = send(
            &server,
            "/v1/followups/ai-cadence",
            json!({ "intent": "start", "customer_id": 9600, "preview": preview }),
        )
        .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], "validation_failed");
    }
    // A skipped first message: the rest of the plan is written when it goes out.
    let res = send(
        &server,
        "/v1/followups/ai-cadence",
        json!({ "intent": "start", "customer_id": 9600, "preview": [{
            "delay_minutes": 0, "delay_label": "Immediately", "goal": "x", "channel": "sms",
            "skip": true, "skip_reason": "no", "body": "", "subject": null }] }),
    )
    .await;
    res.assert_status_ok();
    let texts: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT s.custom_text FROM sms_flow_steps s JOIN sms_flows f ON f.id = s.flow_id
          WHERE f.name = 'Generated with AI' AND f.user_id = 100 ORDER BY s.position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(!texts.is_empty());
    assert!(
        texts
            .iter()
            .all(|t| t.as_deref() == Some("__GR_AI_FOLLOWUP__"))
    );
}

#[test_db::test(seed)]
async fn one_message_is_rewritten_unlike_the_rejected_draft(pool: MySqlPool) {
    let (server, model) = setup(&pool).await;
    model
        .replies
        .lock()
        .unwrap()
        .push_back(json!({ "skip": false, "messages": [{
        "skip": false, "subject": "", "body": "Paula, would a quick visit to the showroom help?"
    }] }));
    let res = send(
        &server,
        "/v1/followups/ai-cadence/regenerate",
        json!({
            "customer_id": 9600, "channel": "sms", "goal": "Check in", "delay_minutes": 1440,
            "delay_label": "+1 day", "avoid_body": DRAFTS[0], "avoid_subject": null,
            "avoid_previous": [DRAFTS[1]], "angle": "quote_status"
        }),
    )
    .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({ "body": "Paula, would a quick visit to the showroom help?", "subject": null })
    );
    let prompts = model.prompts.lock().unwrap().clone();
    let user_prompt = prompts[0]["messages"][1]["content"].as_str().unwrap();
    assert!(user_prompt.contains("REGENERATE THIS SINGLE MESSAGE."));
    assert!(user_prompt.contains(DRAFTS[0]) && user_prompt.contains(DRAFTS[1]));

    let res = send(
        &server,
        "/v1/followups/ai-cadence/regenerate",
        json!({ "customer_id": 9600, "channel": "sms", "goal": "", "delay_minutes": 0,
                "delay_label": "", "avoid_body": "x" }),
    )
    .await;
    assert_eq!(res.json::<Value>()["error"], "validation_failed");
}

#[test_db::test(seed)]
async fn a_preview_keeps_the_gaps_signs_the_texts_and_knows_the_offer(pool: MySqlPool) {
    let (server, model) = setup(&pool).await;
    sqlx::raw_sql(
        "UPDATE users SET followup_sms_signature = 'Rita\nAcme Stone' WHERE id = 100;
         INSERT INTO ai_followup_offers (company_id, offer, ends_on, updated_by, updated_at)
           VALUES (100, 'Free sink with any kitchen', NULL, 100, '2026-10-01 09:00:00');",
    )
    .execute(&pool)
    .await
    .unwrap();
    // The model asks for 45 days between every message.
    model.replies.lock().unwrap().push_back(
        json!({ "skip": false, "messages": DRAFTS.iter().map(|body| json!({
        "skip": false, "subject": "Your countertops", "body": body, "delayMinutes": 45 * 1440
    })).collect::<Vec<_>>() }),
    );
    let res = send(
        &server,
        "/v1/followups/ai-cadence/preview",
        json!({ "customer_id": 9600 }),
    )
    .await;
    res.assert_status_ok();
    let preview = res.json::<Value>()["preview"].as_array().unwrap().clone();
    assert!(preview.len() >= 2, "{preview:?}");
    let delays: Vec<i64> = preview
        .iter()
        .map(|m| m["delay_minutes"].as_i64().unwrap())
        .collect();
    // The second message comes 2–4 days after the first; no wait is over 30 days.
    assert!((2 * 1440..=4 * 1440).contains(&delays[1]), "{delays:?}");
    assert!(delays.iter().skip(1).all(|d| *d <= 30 * 1440), "{delays:?}");
    for message in &preview {
        if message["channel"] == "sms" {
            let body = message["body"].as_str().unwrap();
            assert!(body.ends_with("\n\nRita\nAcme Stone"), "{body}");
            assert_eq!(message["generated_body"], message["body"]);
        }
    }
    let prompts = model.prompts.lock().unwrap().clone();
    let user_prompt = prompts[0]["messages"][1]["content"].as_str().unwrap();
    assert!(
        user_prompt.contains("Free sink with any kitchen"),
        "{user_prompt}"
    );
}
