//! `POST /v1/ai/chat`: answers streamed as server-sent events, against an
//! `OpenAI` stand-in that streams (`stream: true`) or calls tools.

use crate::common;

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
        .filter(|(e, d)| e == "message" && d != "[DONE]035d8eba-9f8c-44c5-a1e0-290d1da033f7[/DONE]")
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
    assert_eq!(
        events.last().unwrap().1,
        "[DONE]035d8eba-9f8c-44c5-a1e0-290d1da033f7[/DONE]"
    );
    let used: Value = serde_json::from_str(named(&events, "instructions")[0]).unwrap();
    assert_eq!(used, json!([{ "id": 9100, "title": "Fabrication Prices" }]));

    // The model got this company's instructions the rep may read, and the manual.
    let request = ai.requests.lock().unwrap()[0].clone();
    assert_eq!(request["stream"], true);
    let system = request["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains("PRICE QUERY: Check Instruction 1 first"),
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
            // The model finds it asks about the records, not the app.
            Reply::Text(json!({ "records": true }).to_string()),
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
    let pick = requests[0]["messages"][0]["content"].as_str().unwrap();
    assert!(pick.starts_with("You find where the Granite Manager user manual answers"));
    let first = &requests[1];
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
        serde_json::from_str(requests[2]["messages"][3]["content"].as_str().unwrap()).unwrap();
    assert_eq!(lookup["selected"]["name"], "Alice Lead");
    assert_eq!(lookup["selected"]["assigned_to_you"], true);
    let deals: Value =
        serde_json::from_str(requests[3]["messages"][5]["content"].as_str().unwrap()).unwrap();
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

/// The assistant reads customers, deals and notes: a user who is not an
/// employee (shop, marketing) may not ask for it.
#[test_db::test(seed)]
async fn the_assistant_is_for_employees(pool: MySqlPool) {
    let (server, ai) = setup(&pool, Vec::new()).await;
    server
        .post("/v1/ai/chat")
        .with_session(sessions::VIEWER)
        .json(&json!({ "query": "find customer Alice", "mode": "assistant" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    assert!(ai.requests.lock().unwrap().is_empty());
}

/// A non-employee's question about records is answered from the
/// instructions: no tool that reads customers, deals or notes is offered,
/// and no record reaches the model.
#[test_db::test(seed)]
async fn a_non_employees_record_question_gets_no_assistant(pool: MySqlPool) {
    let replies = (0..6)
        .map(|_| Reply::Text("I can only help with instructions.".into()))
        .collect();
    let (server, ai) = setup(&pool, replies).await;
    for mode in [None, Some("instructions"), Some("anything")] {
        let mut body = json!({ "query": "find customer Alice and email her", "isNew": true });
        if let Some(mode) = mode {
            body["mode"] = json!(mode);
        }
        server
            .post("/v1/ai/chat")
            .with_session(sessions::VIEWER)
            .json(&body)
            .await
            .assert_status_ok();
    }
    let requests = ai.requests.lock().unwrap().clone();
    assert!(!requests.is_empty());
    for request in &requests {
        assert!(request.get("tools").is_none_or(Value::is_null), "{request}");
        let text = request["messages"].to_string();
        assert!(!text.contains("You are the AI assistant inside Granite Manager"));
        assert!(
            !text.contains("Alice Lead") && !text.contains("alice@example.com"),
            "{text}"
        );
    }
}

const SPECIAL_ORDERS: &str = "
INSERT INTO instructions (id, title, rich_text, company_id) VALUES
  (9110, 'Special Orders', '<p>Take a 50% deposit, then order the slabs from the supplier.</p>', 100);
";

const CLARIFY_QUESTION: &str =
    "Do you mean how to do this in Granite Manager, or your company's own process?";

/// The model's messages' contents.
fn contents(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap_or_default().to_string())
        .collect()
}

const SPECIAL_ORDER_QUESTION: &str = "Do you mean a special-order slab in a quote, the Special Order calculator, or your company's own process?";

/// "How do I make a special order?" is a special-order slab in a quote, the
/// Special Order calculator or the company's own process: the chat asks one
/// question with all three instead of guessing.
async fn asked_back(server: &TestServer, pool: &MySqlPool, ai: &OpenAi) {
    let events = ask(
        server,
        json!({ "query": "how do I make a special order?", "isNew": true }),
    )
    .await;
    assert_eq!(answer(&events), SPECIAL_ORDER_QUESTION);
    let choices: Value = serde_json::from_str(named(&events, "clarify")[0]).unwrap();
    assert_eq!(
        choices,
        json!([
            { "label": "Special-order slab in a quote", "source": "manual" },
            { "label": "Special Order calculator", "source": "manual" },
            { "label": "Our company's process", "source": "company" },
        ])
    );
    assert!(ai.requests.lock().unwrap().is_empty());
    let saved = history(pool).await;
    assert_eq!(
        saved.last().unwrap()["content"],
        format!(
            "{SPECIAL_ORDER_QUESTION}\n\n[[CLARIFY:how do I make a special order?|Special-order slab in a quote=quote-drawing-color-slab-layout|Special Order calculator=samples-special-order|Our company's process]]"
        )
    );
}

/// The model's pick of the manual's sections, before the answer.
fn picks(sections: &[&str]) -> Reply {
    Reply::Text(json!({ "sections": sections }).to_string())
}

#[test_db::test(seed)]
async fn picking_the_quote_answers_the_question_from_the_manual(pool: MySqlPool) {
    sqlx::raw_sql(SPECIAL_ORDERS).execute(&pool).await.unwrap();
    let reply = "Open the quote, click **Edit drawing** and type the color. [[INSTRUCTION:1]]";
    let (server, ai) = setup(&pool, vec![Reply::Text(reply.into())]).await;
    asked_back(&server, &pool, &ai).await;

    let events = ask(
        &server,
        json!({ "query": "Special-order slab in a quote", "isNew": false }),
    )
    .await;

    assert_eq!(answer(&events), reply);
    assert!(named(&events, "clarify").is_empty());
    // A manual answer has no company instruction sources, cited or not.
    assert!(named(&events, "instructions").is_empty());
    assert!(named(&events, "images").is_empty());
    assert_eq!(named(&events, "crmManual"), ["true"]);
    // The choice has its sections: no pick.
    let requests = ai.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let sent = contents(&requests[0]);
    assert!(sent.iter().any(|c| c.contains("CRM USER MANUAL:")
        && c.contains("#### Special order (a color not in the list)")));
    assert!(
        sent.iter()
            .all(|c| !c.contains("Here are the instructions you can use"))
    );
    assert!(sent.iter().all(|c| !c.contains("[[CLARIFY:")));
    assert!(sent.last().unwrap().contains(
        "Question: how do I make a special order? (I mean Special-order slab in a quote in Granite Manager.)"
    ));
}

/// "In Granite Manager" typed to the question of three: the manual, which
/// still has to ask which special order.
#[test_db::test(seed)]
async fn typing_the_app_to_the_question_of_three_asks_which_feature(pool: MySqlPool) {
    sqlx::raw_sql(SPECIAL_ORDERS).execute(&pool).await.unwrap();
    let (server, ai) = setup(&pool, Vec::new()).await;
    asked_back(&server, &pool, &ai).await;

    let events = ask(
        &server,
        json!({ "query": "In Granite Manager", "isNew": false }),
    )
    .await;

    assert_eq!(
        answer(&events),
        "Do you mean a special-order slab in a quote, or the Special Order calculator?"
    );
    let choices: Value = serde_json::from_str(named(&events, "clarify")[0]).unwrap();
    assert_eq!(
        choices,
        json!([
            { "label": "Special-order slab in a quote", "source": "manual" },
            { "label": "Special Order calculator", "source": "manual" },
        ])
    );
    assert!(ai.requests.lock().unwrap().is_empty());
}

#[test_db::test(seed)]
async fn picking_the_company_s_process_answers_it_from_the_instructions(pool: MySqlPool) {
    sqlx::raw_sql(SPECIAL_ORDERS).execute(&pool).await.unwrap();
    let reply = "Take a **50%** deposit, then order the slabs. [[INSTRUCTION:1]]";
    let (server, ai) = setup(&pool, vec![Reply::Text(reply.into())]).await;
    asked_back(&server, &pool, &ai).await;

    let events = ask(&server, json!({ "query": "company", "isNew": false })).await;

    assert_eq!(answer(&events), reply);
    let used: Value = serde_json::from_str(named(&events, "instructions")[0]).unwrap();
    assert_eq!(used, json!([{ "id": 9110, "title": "Special Orders" }]));
    let request = ai.requests.lock().unwrap()[0].clone();
    let sent = contents(&request);
    assert!(sent.iter().any(|c| c.contains("TITLE: Special Orders")));
    assert!(sent.iter().all(|c| !c.contains("CRM USER MANUAL:")));
    assert!(sent.last().unwrap().contains(
        "Question: how do I make a special order? (I mean our company's own process, not how to use Granite Manager.)"
    ));
    // The clarification gave way to the question in the saved conversation.
    let saved = history(&pool).await;
    assert!(
        saved
            .iter()
            .all(|m| !m["content"].as_str().unwrap().contains("[[CLARIFY:"))
    );
}

#[test_db::test(seed)]
async fn a_special_order_in_a_quote_goes_to_the_manual_without_asking(pool: MySqlPool) {
    sqlx::raw_sql(SPECIAL_ORDERS).execute(&pool).await.unwrap();
    let reply =
        "Open the quote, click **Edit drawing**, go to **Color & Edge**. [[INSTRUCTION:none]]";
    let (server, ai) = setup(&pool, vec![Reply::Text(reply.into())]).await;

    let events = ask(
        &server,
        json!({ "query": "how to make a special order in quote", "isNew": true }),
    )
    .await;

    assert_eq!(answer(&events), reply);
    assert!(named(&events, "clarify").is_empty());
    assert!(named(&events, "instructions").is_empty());
    // "quote" says which special order: its section, no pick.
    assert_eq!(ai.requests.lock().unwrap().len(), 1);
    let request = ai.requests.lock().unwrap()[0].clone();
    assert!(
        contents(&request)
            .iter()
            .any(|c| c.contains("#### Special order (a color not in the list)"))
    );
}

/// The model reads the manual's table of contents and picks the sections;
/// only those (and the core ones) reach the answer, whatever the typos.
#[test_db::test(seed)]
async fn the_model_picks_the_manual_s_sections_for_the_answer(pool: MySqlPool) {
    let reply =
        "Open **Edit drawing**, step **5 Color & Edge**, **Slabs & Layout**, **Auto layout**.";
    let (server, ai) = setup(
        &pool,
        vec![
            picks(&["quote-drawing-color-slab-layout", "no-such-section"]),
            Reply::Text(reply.into()),
        ],
    )
    .await;

    let events = ask(
        &server,
        json!({ "query": "How to make a layout", "isNew": true }),
    )
    .await;

    assert_eq!(answer(&events), reply);
    assert_eq!(named(&events, "crmManual"), ["true"]);
    let requests = ai.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let pick = contents(&requests[0]);
    assert!(pick[0].contains("[quote-drawing-color-slab-layout] "));
    assert!(pick[0].contains("[admin-phone-telnyx] "));
    assert_eq!(requests[0]["reasoning_effort"], "none");
    let manual = contents(&requests[1]).join("\n");
    assert!(manual.contains("### Slabs & Layout"));
    assert!(manual.contains("## How the application is organized"));
    // Not the portals' overviews the manual falls back to.
    assert!(!manual.contains("## Shop portal (`/shop`)"));
    assert!(!manual.contains("## Employee portal (`/employee`) — feature map"));
    // Short steps, with room to write them: reasoning took the 1024 tokens.
    let answering = &requests[1];
    assert_eq!(answering["reasoning_effort"], "medium");
    assert_eq!(answering["max_completion_tokens"], 4000);
    assert!(contents(answering).last().unwrap().starts_with(
        "Answer how to do this in Granite Manager from the CRM user manual: short numbered steps"
    ));
    assert!(
        contents(answering)
            .last()
            .unwrap()
            .ends_with("Question: How to make a layout\n\nAnswer:")
    );
}

/// A question that fits several features is asked back with the ones there
/// are, without the model; the pick is answered from that feature's sections.
#[test_db::test(seed)]
async fn turning_off_notifications_asks_which_ones_first(pool: MySqlPool) {
    let reply =
        "Click the bell, then the gear, and switch off **Emails** under **Telegram alerts**.";
    let (server, ai) = setup(&pool, vec![Reply::Text(reply.into())]).await;

    let events = ask(
        &server,
        json!({ "query": "How to turn off notifications", "isNew": true }),
    )
    .await;
    let question = answer(&events);
    assert!(
        question.starts_with("Which notifications do you mean"),
        "{question}"
    );
    let choices: Value = serde_json::from_str(named(&events, "clarify")[0]).unwrap();
    let labels: Vec<&str> = choices
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            assert_eq!(c["source"], "manual");
            c["label"].as_str().unwrap()
        })
        .collect();
    assert_eq!(
        labels,
        [
            "Bell in the header",
            "Desktop pop-ups",
            "Telegram alerts",
            "Granite Phone app",
            "Phone ringing",
            "Reminders to customers"
        ]
    );
    assert!(named(&events, "crmManual").is_empty());
    assert!(ai.requests.lock().unwrap().is_empty());
    let saved = history(&pool).await;
    let asked = saved.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        asked.ends_with("[[CLARIFY:How to turn off notifications|Bell in the header=notifications|Desktop pop-ups=notifications|Telegram alerts=notifications,my-account-telegram|Granite Phone app=notifications,admin-phone-telnyx|Phone ringing=notifications,phone-calls,admin-phone-telnyx|Reminders to customers=schedule-notifications,notifications]]"),
        "{asked}"
    );

    let events = ask(
        &server,
        json!({ "query": "Telegram alerts", "isNew": false }),
    )
    .await;

    assert_eq!(answer(&events), reply);
    assert_eq!(named(&events, "crmManual"), ["true"]);
    let requests = ai.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "no pick: the choice has its sections");
    let sent = contents(&requests[0]);
    assert!(sent.iter().any(|c| c.contains("### Telegram alerts")
        && c.contains("#### Choose which alerts go to Telegram")));
    assert!(sent.last().unwrap().contains(
        "Question: How to turn off notifications (I mean Telegram alerts in Granite Manager.)"
    ));
    assert!(sent.iter().all(|c| !c.contains("[[CLARIFY:")));
}

/// The model may ask back too, when the question fits features the clarify
/// table doesn't know.
#[test_db::test(seed)]
async fn the_model_asks_back_when_a_question_fits_several_features(pool: MySqlPool) {
    let asked = json!({ "clarify": {
        "question": "Do you mean email templates or SMS templates?",
        "choices": [
            { "label": "Email templates", "sections": ["admin-automations"] },
            { "label": "SMS templates", "sections": ["employee-sms"] }
        ]
    } });
    let reply = "Open **CRM > SMS**, click the template icon and **Add a Template**.";
    let (server, ai) = setup(
        &pool,
        vec![Reply::Text(asked.to_string()), Reply::Text(reply.into())],
    )
    .await;

    let events = ask(
        &server,
        json!({ "query": "how do I add a template", "isNew": true }),
    )
    .await;
    assert_eq!(
        answer(&events),
        "Do you mean email templates or SMS templates?"
    );
    let choices: Value = serde_json::from_str(named(&events, "clarify")[0]).unwrap();
    assert_eq!(
        choices,
        json!([
            { "label": "Email templates", "source": "manual" },
            { "label": "SMS templates", "source": "manual" }
        ])
    );

    let events = ask(&server, json!({ "query": "SMS templates", "isNew": false })).await;
    assert_eq!(answer(&events), reply);
    let requests = ai.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let sent = contents(&requests[1]).join("\n");
    assert!(sent.contains("### SMS templates"));
    assert!(sent.contains("(I mean SMS templates in Granite Manager.)"));
}

/// A feature the topic table names is answered, even when the model would
/// ask back: "layout" in this app is the quote's slab layout.
#[test_db::test(seed)]
async fn a_feature_the_topic_table_names_is_not_asked_back(pool: MySqlPool) {
    let asked = json!({ "clarify": {
        "question": "Do you mean a quote drawing layout or an AI Design picture?",
        "choices": [
            { "label": "Quote drawing layout", "sections": ["quote-drawing"] },
            { "label": "AI Design picture", "sections": ["ai-chat-ai-design"] }
        ]
    } });
    let reply = "Open **Slabs & Layout** and click **Auto layout**.";
    let (server, ai) = setup(
        &pool,
        vec![Reply::Text(asked.to_string()), Reply::Text(reply.into())],
    )
    .await;
    let events = ask(
        &server,
        json!({ "query": "How to make a layout", "isNew": true }),
    )
    .await;
    assert_eq!(answer(&events), reply);
    assert!(named(&events, "clarify").is_empty());
    let requests = ai.requests.lock().unwrap().clone();
    assert!(
        contents(&requests[1])
            .iter()
            .any(|c| c.contains("### Slabs & Layout"))
    );
}

/// A how-to question that names records ("can I delete a deal?") is the
/// manual's, not the assistant's.
#[test_db::test(seed)]
async fn a_how_to_about_records_is_answered_from_the_manual(pool: MySqlPool) {
    let reply = "Open the deal and click **Delete**.";
    let (server, ai) = setup(
        &pool,
        vec![
            picks(&["employee-deals-deal-board-pipeline"]),
            Reply::Text(reply.into()),
        ],
    )
    .await;
    let events = ask(
        &server,
        json!({ "query": "can i delete a deal?", "isNew": true }),
    )
    .await;
    assert_eq!(answer(&events), reply);
    let requests = ai.requests.lock().unwrap().clone();
    for request in &requests {
        assert!(request.get("tools").is_none_or(Value::is_null), "{request}");
    }
    assert!(
        contents(&requests[1])
            .iter()
            .any(|c| c.contains("### Deleting a deal"))
    );
}

/// Admins and super admins who are not marked employees keep the assistant.
#[test_db::test(seed)]
async fn admins_keep_the_assistant(pool: MySqlPool) {
    for session in [sessions::ADMIN, sessions::SUPER_ADMIN] {
        let (server, ai) = setup(&pool, vec![Reply::Text("Here you go.".into())]).await;
        server
            .post("/v1/ai/chat")
            .with_session(session)
            .json(&json!({ "query": "find customer Alice", "mode": "assistant", "isNew": true }))
            .await
            .assert_status_ok();
        let requests = ai.requests.lock().unwrap().clone();
        let system = requests[0]["messages"][0]["content"].as_str().unwrap();
        assert!(
            system.starts_with("You are the AI assistant inside Granite Manager"),
            "{system}"
        );
    }
}

/// A company with no instruction about the question (only the AI follow-up
/// one, and one about sealing) is not asked whether it means its own
/// process: the manual answers, with no instruction sources.
#[test_db::test(seed)]
async fn a_company_with_no_instruction_about_it_is_not_asked_about_its_process(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO instructions (id, title, rich_text, company_id) VALUES
           (9120, 'AI follow-up instruction', '<p>Follow up every two days by email.</p>', 100),
           (9121, 'Sealing', '<p>Seal every counter once a year.</p>', 100);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let reply =
        "In **Color**, type the color's name and enter its **$ Price /sq ft**. [[INSTRUCTION:1]]";
    let (server, ai) = setup(&pool, vec![Reply::Text(reply.into())]).await;

    let events = ask(
        &server,
        json!({ "query": "How to make a special order?", "isNew": true }),
    )
    .await;
    let asked = answer(&events);
    assert_eq!(
        asked,
        "Do you mean a special-order slab in a quote, or the Special Order calculator?"
    );
    assert!(!asked.contains("company"));
    let choices: Value = serde_json::from_str(named(&events, "clarify")[0]).unwrap();
    assert!(
        choices
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["source"] == "manual")
    );
    assert!(ai.requests.lock().unwrap().is_empty());

    let events = ask(
        &server,
        json!({ "query": "Special-order slab in a quote", "isNew": false }),
    )
    .await;
    assert_eq!(answer(&events), reply);
    assert!(named(&events, "instructions").is_empty());
    assert!(named(&events, "images").is_empty());
    assert_eq!(named(&events, "crmManual"), ["true"]);
}

/// A company with an instruction about it may mean its own process: asked.
/// The generic question stays for what the clarify table doesn't know.
#[test_db::test(seed)]
async fn a_company_with_an_instruction_about_it_is_asked(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO instructions (id, title, rich_text, company_id) VALUES
           (9122, 'Walk-in lead steps', '<p>Greet them, then fill in the walk-in form.</p>', 100);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (server, ai) = setup(&pool, Vec::new()).await;
    let events = ask(
        &server,
        json!({ "query": "how do I add a walk-in lead?", "isNew": true }),
    )
    .await;
    assert_eq!(answer(&events), CLARIFY_QUESTION);
    let choices: Value = serde_json::from_str(named(&events, "clarify")[0]).unwrap();
    assert_eq!(
        choices,
        json!([
            { "label": "In Granite Manager", "source": "manual" },
            { "label": "Our company's process", "source": "company" },
        ])
    );
    assert!(ai.requests.lock().unwrap().is_empty());
}

/// Like company 1's handbook: a page with pictures that mentions colors and
/// options, but nothing about special orders in the app.
const HANDBOOK: &str = "
INSERT INTO instructions (id, title, rich_text, company_id) VALUES
  (9130, 'Salesrep Handbook - 12.3: Cabinets - Renderings and Moraware quotes',
   '<p>Cabinet renderings: pick the door style and the color option, then send the rendering.</p><img src=\"https://img.test/moraware.png\"><p>Moraware quotes have no other option for cabinets.</p>', 100),
  (9131, 'Special Orders', '<p>Take a 50% deposit, then order the slabs from the supplier.</p>', 100);
";

/// The chat so far: a how-to the manual answered.
async fn manual_answer_before(pool: &MySqlPool) {
    let earlier = json!([
        { "role": "system", "content": "Here are the instructions you can use, each TITLE is linked to its own BODY:" },
        { "role": "system", "content": "CRM USER MANUAL:\n\n## Quotes" },
        { "role": "user", "content": "Answer how to do this in Granite Manager from the CRM user manual: short numbered steps with the exact labels, only what the question needs.\n\nQuestion: how do I add a special order slab to a quote\n\nAnswer:" },
        { "role": "assistant", "content": "1. In **Color**, type the color's name.\n2. Enter its **$ Price /sq ft**." }
    ]);
    sqlx::query("INSERT INTO chat_history (history, user_id) VALUES (?, 100)")
        .bind(earlier.to_string())
        .execute(pool)
        .await
        .unwrap();
}

/// A follow-up to a manual answer ("what does it mean?", "there is no such
/// option") stays on the manual: no company sources or their pictures, even
/// when the model cites one. A follow-up about the company's own process
/// goes to its instructions.
#[test_db::test(seed)]
async fn a_follow_up_to_a_manual_answer_stays_on_the_manual(pool: MySqlPool) {
    sqlx::raw_sql(HANDBOOK).execute(&pool).await.unwrap();
    manual_answer_before(&pool).await;
    let reply = "Type the color's name in **Color**; there is no option to pick. [[INSTRUCTION:1]]";
    let company = "Take a **50%** deposit, then order the slabs. [[INSTRUCTION:1]]";
    let (server, ai) = setup(
        &pool,
        vec![
            picks(&["quote-drawing-color-slab-layout"]),
            Reply::Text(reply.into()),
            Reply::Text(company.into()),
        ],
    )
    .await;

    let events = ask(
        &server,
        json!({ "query": "what does it mean other color?? there is no such option", "isNew": false }),
    )
    .await;
    assert_eq!(answer(&events), reply);
    assert!(named(&events, "instructions").is_empty());
    assert!(named(&events, "images").is_empty());
    assert_eq!(named(&events, "crmManual"), ["true"]);
    let requests = ai.requests.lock().unwrap().clone();
    let sent = contents(&requests[1]);
    assert!(
        sent.iter()
            .any(|c| c.contains("#### Special order (a color not in the list)"))
    );
    assert!(
        sent.last()
            .unwrap()
            .starts_with("Answer how to do this in Granite Manager from the CRM user manual")
    );

    let events = ask(
        &server,
        json!({ "query": "what is our special order deposit policy", "isNew": false }),
    )
    .await;
    assert_eq!(answer(&events), company);
    assert!(named(&events, "crmManual").is_empty());
    let used: Value = serde_json::from_str(named(&events, "instructions")[0]).unwrap();
    assert_eq!(used, json!([{ "id": 9131, "title": "Special Orders" }]));
}

/// A source only for an instruction the model cited and that has what the
/// answer says: none when it cites nothing (no guess from the question's
/// words), none when the cited one doesn't have the answer.
#[test_db::test(seed)]
async fn a_source_is_only_an_instruction_the_answer_cites_and_uses(pool: MySqlPool) {
    sqlx::raw_sql(INSTRUCTIONS).execute(&pool).await.unwrap();
    let (server, _) = setup(
        &pool,
        vec![
            Reply::Text("Fabrication is **$25** per sq ft for granite.".into()),
            Reply::Text("Open the quote and click **Email**. [[INSTRUCTION:1]]".into()),
            Reply::Text("Fabrication is **$25** per sq ft for granite. [[INSTRUCTION:1]]".into()),
        ],
    )
    .await;
    for (reply, cited) in [(0, false), (1, false), (2, true)] {
        let events = ask(
            &server,
            json!({ "query": "what is the price for fabrication", "isNew": true }),
        )
        .await;
        assert_eq!(
            !named(&events, "instructions").is_empty(),
            cited,
            "reply {reply}"
        );
        if cited {
            let used: Value = serde_json::from_str(named(&events, "instructions")[0]).unwrap();
            assert_eq!(used, json!([{ "id": 9100, "title": "Fabrication Prices" }]));
            assert_eq!(
                named(&events, "images"),
                [r#"["https://img.test/fab.png"]"#]
            );
        } else {
            assert!(named(&events, "images").is_empty(), "reply {reply}");
        }
    }
}
