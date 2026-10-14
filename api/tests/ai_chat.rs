//! `POST /v1/ai/chat`: answers streamed as server-sent events, against an
//! `OpenAI` stand-in that streams (`stream: true`) or calls tools.

mod common;

use api::ai::Ai;
use api::app;
use api::state::AppState;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// What the stand-in answers, in order: text (streamed or not) or tool calls.
#[derive(Clone)]
enum Reply {
    Text(String),
    Tools(Vec<(String, Value)>),
}

#[derive(Clone, Default)]
struct OpenAi {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn complete(State(ai): State<OpenAi>, Json(request): Json<Value>) -> Response {
    ai.requests.lock().unwrap().push(request.clone());
    let reply = ai
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .expect("a reply queued");
    if request["stream"] == true {
        let Reply::Text(text) = reply else {
            panic!("tools in a stream")
        };
        let mut body = String::new();
        // Several small pieces, like the real stream.
        for piece in text.as_bytes().chunks(7) {
            let piece = String::from_utf8_lossy(piece);
            body.push_str(&format!(
                "data: {}\n\n",
                json!({ "choices": [{ "delta": { "content": piece } }] })
            ));
        }
        body.push_str(&format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({ "choices": [], "usage": { "prompt_tokens": 900, "completion_tokens": 30 } })
        ));
        return ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response();
    }
    let message = match reply {
        Reply::Text(text) => json!({ "role": "assistant", "content": text }),
        Reply::Tools(calls) => json!({
            "role": "assistant", "content": null,
            "tool_calls": calls.iter().enumerate().map(|(i, (name, args))| json!({
                "id": format!("call_{i}"), "type": "function",
                "function": { "name": name, "arguments": args.to_string() },
            })).collect::<Vec<_>>(),
        }),
    };
    Json(json!({
        "model": "gpt-6-luna",
        "choices": [{ "message": message, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 400, "completion_tokens": 20 },
    }))
    .into_response()
}

async fn setup(pool: &MySqlPool, replies: Vec<Reply>) -> (TestServer, OpenAi) {
    let ai = OpenAi::default();
    ai.replies.lock().unwrap().extend(replies);
    let router = Router::new()
        .route("/v1/chat/completions", post(complete))
        .with_state(ai.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN).with_ai(Ai::new(
        Some("sk-test"),
        &format!("{base}/v1/chat/completions"),
    ));
    (TestServer::new(app(state)), ai)
}

/// `(event, data)` of each server-sent event.
fn events(body: &str) -> Vec<(String, String)> {
    body.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let mut event = "message".to_string();
            let mut data: Vec<&str> = Vec::new();
            for line in block.lines() {
                if let Some(name) = line.strip_prefix("event:") {
                    event = name.trim().to_string();
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push(d.strip_prefix(' ').unwrap_or(d));
                }
            }
            (event, data.join("\n"))
        })
        .collect()
}

/// The streamed answer: the `message` pieces before `[DONE]`.
fn answer(events: &[(String, String)]) -> String {
    events
        .iter()
        .filter(|(e, d)| e == "message" && d != "[DONE]")
        .map(|(_, d)| d.as_str())
        .collect()
}

fn named<'a>(events: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter(|(e, _)| e == name)
        .map(|(_, d)| d.as_str())
        .collect()
}

async fn ask(server: &TestServer, body: Value) -> Vec<(String, String)> {
    let res = server
        .post("/v1/ai/chat")
        .with_session(sessions::REP)
        .json(&body)
        .await;
    res.assert_status_ok();
    assert!(
        res.header(header::CONTENT_TYPE)
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    events(&res.text())
}

async fn history(pool: &MySqlPool) -> Vec<Value> {
    let raw: String =
        sqlx::query_scalar("SELECT CAST(history AS CHAR) FROM chat_history WHERE user_id = 100")
            .fetch_one(pool)
            .await
            .unwrap();
    serde_json::from_str(&raw).unwrap()
}

const INSTRUCTIONS: &str = "
INSERT INTO instructions (id, title, rich_text, company_id) VALUES
  (9100, 'Fabrication Prices', '<p>Fabrication is $25 per sq ft for granite.</p><p>Quartz fabrication is $30 per sq ft.</p><img src=\"https://img.test/fab.png\">', 100),
  (9101, 'Sealing', '<p>Seal every counter once a year.</p>', 100),
  (9102, 'Managers only', '<p>Fabrication bonus is $5 per sq ft.</p>', 100),
  (9103, 'Other company', '<p>Fabrication is $99 per sq ft.</p>', 101);
INSERT INTO instruction_access (instruction_id, position_id) VALUES (9102, 2);
";

#[test_db::test(seed)]
async fn an_instructions_answer_streams_with_the_instruction_it_used(pool: MySqlPool) {
    sqlx::raw_sql(INSTRUCTIONS).execute(&pool).await.unwrap();
    let text = "Fabrication is **$25** per sq ft for granite and $30 for quartz. [[INSTRUCTION:1]]";
    let (server, ai) = setup(&pool, vec![Reply::Text(text.into())]).await;
    let events = ask(
        &server,
        json!({ "query": "what is the price for fabrication", "isNew": true }),
    )
    .await;

    assert_eq!(named(&events, "ping").len(), 30);
    assert_eq!(named(&events, "info"), ["Connecting to AI..."]);
    assert_eq!(named(&events, "status"), [r#"{"state":"answering"}"#]);
    assert_eq!(answer(&events), text);
    assert_eq!(events.last().unwrap().1, "[DONE]");
    let used: Value = serde_json::from_str(named(&events, "instructions")[0]).unwrap();
    assert_eq!(used, json!([{ "id": 9100, "title": "Fabrication Prices" }]));

    // The model got this company's instructions the rep may read, and the manual.
    let request = ai.requests.lock().unwrap()[0].clone();
    assert_eq!(request["stream"], true);
    let system = request["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.starts_with("PRICE QUERY: Check Instruction 1 first"),
        "{system}"
    );
    assert!(system.contains("TITLE: Fabrication Prices") && system.contains("TITLE: Sealing"));
    assert!(!system.contains("Managers only") && !system.contains("$99"));
    let manual = request["messages"][1]["content"].as_str().unwrap();
    assert!(manual.contains("CURRENT USER ACCESS") && manual.contains("CRM USER MANUAL:"));
    assert!(
        request["messages"][2]["content"]
            .as_str()
            .unwrap()
            .contains("Question: what is the price for fabrication")
    );

    // Saved without the markers, ready for the next question.
    let saved = history(&pool).await;
    assert_eq!(
        saved.last().unwrap(),
        &json!({ "role": "assistant", "content": "Fabrication is **$25** per sq ft for granite and $30 for quartz." })
    );
    let logged: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ai_usage_log WHERE feature = 'chat' AND output_tokens = 30",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(logged, 1);
}

#[test_db::test(seed)]
async fn a_vague_answer_gets_a_second_look_at_the_other_instructions(pool: MySqlPool) {
    sqlx::raw_sql(INSTRUCTIONS).execute(&pool).await.unwrap();
    let (server, ai) = setup(
        &pool,
        vec![
            Reply::Text("I do not have an instruction for that. [[INSTRUCTION:1]]".into()),
            Reply::Text("Seal every counter once a year. [[INSTRUCTION:1]]".into()),
        ],
    )
    .await;
    let events = ask(
        &server,
        json!({ "query": "how often do we seal a counter, fabrication", "isNew": true }),
    )
    .await;
    let replaced = events
        .iter()
        .position(|(e, _)| e == "replace")
        .expect("replaced");
    assert_eq!(
        answer(&events[replaced..]),
        "Seal every counter once a year. [[INSTRUCTION:1]]"
    );
    let second = ai.requests.lock().unwrap()[1]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!second.contains("TITLE: Fabrication Prices"), "{second}");
}

#[test_db::test(seed)]
async fn a_special_order_is_worked_out_without_the_model(pool: MySqlPool) {
    sqlx::query("UPDATE company SET state_taxes = 0.07 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO chat_history (history, user_id) VALUES (?, 100)")
        .bind(json!([{ "role": "user", "content": "Taj Mahal price" }, { "role": "assistant", "content": "$40 per sqft" }]).to_string())
        .execute(&pool)
        .await
        .unwrap();
    let (server, ai) = setup(&pool, Vec::new()).await;
    let events = ask(
        &server,
        json!({
            "query": "2 slabs, delivery $150", "mode": "priceLists", "isNew": false,
            "specialOrder": { "pricePerSqft": 40, "lengthInches": 126, "widthInches": 63 },
        }),
    )
    .await;
    assert_eq!(
        answer(&events),
        "Price per sqft (with tax & delivery): $44.16\nTotal square feet: 110.25 sqft\nMaterial cost (incl. 7% tax): $4718.70\nDelivery: $150.00\nTotal cost: $4868.70"
    );
    assert!(ai.requests.lock().unwrap().is_empty());
    assert_eq!(history(&pool).await.len(), 4);

    let events = ask(
        &server,
        json!({ "query": "no thanks", "mode": "priceLists", "specialOrder": { "pricePerSqft": 40 } }),
    )
    .await;
    assert_eq!(
        answer(&events),
        "No problem. Let me know if you need anything else."
    );
}

#[test_db::test(seed)]
async fn the_assistant_looks_up_a_customer_and_drafts_an_email(pool: MySqlPool) {
    let (server, ai) = setup(
        &pool,
        vec![
            Reply::Tools(vec![("lookup_customer".into(), json!({ "query": "Alice" }))]),
            Reply::Tools(vec![
                ("get_customer_deals".into(), json!({ "customer_id": 1000 })),
                ("draft_email".into(), json!({ "to_email": "alice@example.com", "subject": "Your quote", "body": "Hi Alice & co" })),
            ]),
            Reply::Text("Alice Lead has a won kitchen deal. I drafted the email.".into()),
        ],
    )
    .await;
    let events = ask(
        &server,
        json!({ "query": "find customer Alice and email her", "isNew": true }),
    )
    .await;
    assert_eq!(
        answer(&events),
        "Alice Lead has a won kitchen deal. I drafted the email."
    );
    let actions: Value = serde_json::from_str(named(&events, "actions")[0]).unwrap();
    assert_eq!(
        actions,
        json!([{ "type": "link", "label": "Open email draft for alice@example.com",
                 "url": "mailto:alice%40example.com?subject=Your%20quote&body=Hi%20Alice%20%26%20co" }])
    );

    let requests = ai.requests.lock().unwrap().clone();
    let first = &requests[0];
    assert_eq!(first["reasoning_effort"], "none");
    let system = first["messages"][0]["content"].as_str().unwrap();
    assert!(system.starts_with("You are the AI assistant inside Granite Manager"));
    assert!(
        system.contains("Rita Rep <rep@acme.test> — employee, users.id 100."),
        "{system}"
    );
    assert!(system.contains("id 101 — Mark Manager <manager@acme.test>"));
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        tools,
        [
            "lookup_customer",
            "get_customer_deals",
            "search_deal_notes",
            "draft_email"
        ]
    );
    // The skills' results went back to the model.
    let lookup: Value =
        serde_json::from_str(requests[1]["messages"][3]["content"].as_str().unwrap()).unwrap();
    assert_eq!(lookup["selected"]["name"], "Alice Lead");
    assert_eq!(lookup["selected"]["assigned_to_you"], true);
    let deals: Value =
        serde_json::from_str(requests[2]["messages"][5]["content"].as_str().unwrap()).unwrap();
    assert_eq!(deals["customer_id"], 1000);
    assert!(
        deals["deals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["title"] == "Alice kitchen" && d["amount"] == "1000.00")
    );
    // The saved history has no system prompt.
    let saved = history(&pool).await;
    assert_eq!(saved[0]["role"], "user");
}

#[test_db::test(seed)]
async fn a_bad_skill_call_is_explained_to_the_model(pool: MySqlPool) {
    let (server, ai) = setup(
        &pool,
        vec![
            Reply::Tools(vec![
                ("get_customer_deals".into(), json!({ "customer_id": -3 })),
                ("search_deal_notes".into(), json!({ "search": "x" })),
                ("unknown_skill".into(), json!({})),
            ]),
            Reply::Text("Sorry.".into()),
        ],
    )
    .await;
    ask(
        &server,
        json!({ "query": "deals", "mode": "assistant", "isNew": true }),
    )
    .await;
    let second = ai.requests.lock().unwrap()[1].clone();
    let results: Vec<Value> = second["messages"].as_array().unwrap()[3..]
        .iter()
        .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(
        results[0]["error"],
        "Invalid arguments for get_customer_deals."
    );
    assert_eq!(results[1]["issues"][0]["path"], "search");
    assert_eq!(results[2]["error"], "Unknown skill: unknown_skill");
}

#[test_db::test(seed)]
async fn the_chat_needs_a_session_and_a_question(pool: MySqlPool) {
    let (server, _) = setup(&pool, Vec::new()).await;
    server
        .post("/v1/ai/chat")
        .json(&json!({ "query": "hi" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post("/v1/ai/chat")
        .with_session(sessions::REP)
        .json(&json!({ "query": "  " }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}
