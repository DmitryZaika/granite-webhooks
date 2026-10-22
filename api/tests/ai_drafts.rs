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

/// A server whose `OpenAI` stub always answers `reply`, and the requests it got.
async fn server(pool: &MySqlPool, reply: &'static str) -> (TestServer, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::clone(&seen);
    let router = Router::new().route(
        "/chat",
        post(move |Json(body): Json<Value>| {
            let calls = Arc::clone(&calls);
            async move {
                calls.lock().unwrap().push(body);
                Json(json!({
                    "choices": [{ "message": { "content": reply } }],
                    "usage": { "prompt_tokens": 10, "completion_tokens": 5 },
                }))
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
async fn an_admin_drafts_an_email_template(pool: MySqlPool) {
    let (server, seen) = server(
        &pool,
        "```json\n{\"template_name\":\"Follow-up\",\"template_subject\":\"Hi {{customer.first_name}}\",\"template_body\":\"<p>Hello</p>\"}\n```",
    )
    .await;
    let body: Value = server
        .post("/v1/ai/email-template")
        .with_session(sessions::ADMIN)
        .json(&json!({ "prompt": "follow up after an estimate" }))
        .await
        .json();
    assert_eq!(
        body,
        json!({ "template_name": "Follow-up", "template_subject": "Hi {{customer.first_name}}", "template_body": "<p>Hello</p>" })
    );
    let sent = seen.lock().unwrap().clone();
    assert_eq!(
        sent[0]["messages"][1]["content"],
        "Generate an email template based on this description: \"follow up after an estimate\""
    );
    server
        .post("/v1/ai/email-template")
        .with_session(sessions::REP)
        .json(&json!({ "prompt": "x" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn a_reply_without_a_template_is_a_bad_gateway(pool: MySqlPool) {
    let (server, _) = server(&pool, "Sorry, I can't.").await;
    server
        .post("/v1/ai/email-template")
        .with_session(sessions::ADMIN)
        .json(&json!({ "prompt": "follow up" }))
        .await
        .assert_status(StatusCode::BAD_GATEWAY);
}

#[test_db::test(seed)]
async fn an_invoice_edit_keeps_only_allowed_changes(pool: MySqlPool) {
    let reply = r#"{"invoice":{"id":"9","syncToken":"7","docNumber":"1001","txnDate":null,"dueDate":"2026-06-01","totalAmt":null,"balance":null,"currency":null,"customerMemo":"Thanks!","privateNote":null,"billAddr":null,"shipAddr":null,"lines":[
        {"id":"1","lineNum":1,"detailType":"SalesItemLineDetail","description":"Quartz","amount":1,"qty":2,"unitPrice":60,"itemRefValue":"99","itemRefName":"Hacked","editable":true},
        {"id":"new:1","lineNum":null,"detailType":"SalesItemLineDetail","description":null,"amount":null,"qty":1,"unitPrice":40,"itemRefValue":"3","itemRefName":null,"editable":true},
        {"id":"new:2","lineNum":null,"detailType":"SalesItemLineDetail","description":"Free","amount":0,"qty":1,"unitPrice":0,"itemRefValue":"777","itemRefName":null,"editable":true}
    ]}}"#;
    let (server, seen) = server(&pool, reply).await;
    let line = |id: &str, kind: &str, item: Option<&str>, editable: bool| {
        json!({ "id": id, "line_num": null, "detail_type": kind, "description": "x", "amount": 120, "qty": 1,
                "unit_price": 120, "item_ref_value": item, "item_ref_name": item.map(|_| "Stone"), "editable": editable })
    };
    let invoice = json!({
        "id": "9", "sync_token": "1", "doc_number": "1001", "txn_date": "2026-05-01", "due_date": null,
        "total_amt": 120, "balance": 120, "currency": "USD", "customer_memo": null, "private_note": "keep?",
        "bill_addr": null, "ship_addr": null,
        "lines": [
            line("1", "SalesItemLineDetail", Some("3"), true),
            line("2", "DescriptionOnly", None, true),
            line("3", "SubTotalLineDetail", None, false),
        ],
    });
    let body: Value = server
        .post("/v1/ai/invoice-edit")
        .with_session(sessions::REP)
        .json(
            &json!({ "invoice": invoice, "instruction": "two of them, drop the note, add a sink" }),
        )
        .await
        .json();
    let edited = &body["invoice"];
    assert_eq!(edited["sync_token"], "1");
    assert_eq!(edited["txn_date"], "2026-05-01");
    assert_eq!(edited["due_date"], "2026-06-01");
    assert_eq!(edited["customer_memo"], "Thanks!");
    assert_eq!(edited["private_note"], Value::Null);
    let lines = edited["lines"].as_array().unwrap();
    let ids: Vec<&str> = lines
        .iter()
        .map(|line| line["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["1", "3", "new:1"]);
    assert_eq!(lines[0]["amount"], 120.0);
    assert_eq!(lines[0]["qty"], 2.0);
    assert_eq!(lines[0]["item_ref_value"], "3");
    assert_eq!(lines[2]["item_ref_name"], "Stone");
    assert_eq!(lines[2]["description"], "Stone");
    let sent = seen.lock().unwrap().clone();
    let user: Value =
        serde_json::from_str(sent[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(user["invoice"]["syncToken"], "1");
    assert_eq!(user["invoice"]["lines"][0]["itemRefValue"], "3");
    assert_eq!(sent[0]["response_format"]["type"], "json_object");
}

#[test_db::test(seed)]
async fn a_question_about_a_customer_cites_its_sources(pool: MySqlPool) {
    let (server, seen) = server(
        &pool,
        r#"{"answer":" We texted her a quote. ","sourceKeys":["sms:8301","sms:8301","bogus:1"]}"#,
    )
    .await;
    let body: Value = server
        .post("/v1/customers/1000/activity-search")
        .with_session(sessions::REP)
        .json(&json!({ "question": "Did we send a quote?" }))
        .await
        .json();
    assert_eq!(body["answer"], "We texted her a quote.");
    let sources = body["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["key"], "sms:8301");
    assert_eq!(sources[0]["href"], "/employee/cloudtalk/thread/3175550100");
    assert!(sources[0]["label"].as_str().unwrap().starts_with("SMS · "));
    let sent = seen.lock().unwrap().clone();
    let prompt = sent[0]["messages"][1]["content"].as_str().unwrap();
    assert!(prompt.starts_with("QUESTION:\nDid we send a quote?\n\nSOURCES:\n"));
    assert!(prompt.contains("[Sales Rep]: Quote"));
    server
        .post("/v1/customers/1000/activity-search")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "question": "Did we send a quote?" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .post("/v1/customers/1000/activity-search")
        .with_session(sessions::REP)
        .json(&json!({ "question": "?" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn the_agreed_estimate_time_is_read_from_the_conversation(pool: MySqlPool) {
    let (server, seen) = server(
        &pool,
        r#"{"start":"2099-05-01T10:00","end":null,"evidence":"Friday at 10 works"}"#,
    )
    .await;
    let body: Value = server
        .get("/v1/customers/1000/estimate-time?tz=America/New_York")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        body,
        json!({ "time": { "start": "2099-05-01T10:00", "end": null, "evidence": "Friday at 10 works" } })
    );
    let sent = seen.lock().unwrap().clone();
    assert_eq!(sent[0]["reasoning_effort"], "low");
    let prompt = sent[0]["messages"][1]["content"].as_str().unwrap();
    assert!(prompt.starts_with("Now: "));
    assert!(prompt.contains("] Sales rep (sms): Hi Alice"));
    server
        .get("/v1/customers/1000/estimate-time")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn call_activities_come_from_the_transcript(pool: MySqlPool) {
    let (server, seen) = server(
        &pool,
        r#"{"activities":[{"name":"Call back in two or three weeks","deadline":null},{"name":"Send granite inventory link","deadline":null}]}"#,
    )
    .await;
    let body: Value = server
        .post("/v1/ai/call-activities")
        .with_session(sessions::REP)
        .json(&json!({
            "transcript": "Would you like me to call you in two or three weeks? We are in litigation. I'll send you the inventory link a little bit later.",
            "call_started_at": "2026-11-02T15:00:00.000Z",
            "committed_at": "2026-11-02T15:30:00.000Z",
        }))
        .await
        .json();
    assert_eq!(
        body,
        json!({ "activities": [
            { "name": "Send granite inventory link", "deadline": "2026-11-02" },
            { "name": "Call back, check if litigation is finished", "deadline": "2026-11-16" },
        ] })
    );
    let sent = seen.lock().unwrap().clone();
    let system = sent[0]["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.ends_with("Call date/time for relative deadlines (ISO): 2026-11-02T15:00:00.000Z")
    );
    // A greeting-only voicemail needs no AI (the CRM renames its activity too).
    let voicemail: Value = server
        .post("/v1/ai/call-activities")
        .with_session(sessions::REP)
        .json(&json!({ "transcript": "Please leave a message after the tone.", "is_voicemail": true }))
        .await
        .json();
    assert_eq!(
        voicemail,
        json!({ "activities": [{ "name": "Call back, follow up", "deadline": null }] })
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
}
