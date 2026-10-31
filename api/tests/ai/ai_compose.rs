//! The composer's inline autocomplete (`/v1/ai/compose/*`) against an
//! `OpenAI` stand-in.

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
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// Alice (1000, Rita's lead, 317-555-0100) texted Rita.

const FIXTURE: &str = "
UPDATE users SET cloudtalk_phone_number = '3175550000' WHERE id = 100;
INSERT INTO cloudtalk_sms (sender, recipient, text, direction, status, company_id, created_date, phone_digits)
  VALUES ('3175550100', '3175550000', 'Can you come measure on Friday?', 'inbound', 'received', 100,
          UTC_TIMESTAMP() - INTERVAL 1 HOUR, '3175550100');
";

#[derive(Clone, Default)]
struct Model {
    replies: Arc<Mutex<VecDeque<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn complete(State(model): State<Model>, Json(request): Json<Value>) -> Json<Value> {
    model.requests.lock().unwrap().push(request);
    let content = model
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_default();
    Json(json!({
        "model": "gpt-6-luna",
        "choices": [{ "message": { "content": content }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 400, "completion_tokens": 8 }
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
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN).with_ai(Ai::new(
        Some("sk-test"),
        &format!("{base}/v1/chat/completions"),
    ));
    (TestServer::new(app(state)), model)
}

impl Model {
    fn reply(&self, text: &str) {
        self.replies.lock().unwrap().push_back(text.into());
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

#[test_db::test(seed)]
async fn the_draft_is_continued_from_the_conversation(pool: MySqlPool) {
    let (server, model) = setup(&pool).await;
    model.reply("\"day works, see you at 10!\"");
    let res = server
        .post("/v1/ai/compose/autocomplete")
        .with_session(sessions::REP)
        .json(&json!({ "channel": "sms", "customer_id": 1000, "phone_digits": "3175550100", "draft": "Fri" }))
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({ "completion": "day works, see you at 10!" })
    );

    let request = &model.requests()[0];
    assert_eq!(request["model"], "gpt-6-luna");
    assert_eq!(request["reasoning_effort"], "none");
    assert_eq!(request["max_completion_tokens"], 60);
    assert_eq!(request["prompt_cache_key"], "compose-autocomplete:100");
    let system = request["messages"][0]["content"].as_str().unwrap();
    for line in [
        "\n- Absolute Black (granite; Black): $75/sq ft, slabs 126.5x63 in, 3 slabs available, on sale\n",
        "\n- Calacatta Gold (quartz; White): priced by the slab, slabs 120x55 in, 1 remnant\n",
        "\n- Sold Out Marble (marble): $90/sq ft, sold out\n",
        "\n- Regular Stock Quartzite (quartzite): $110/sq ft, slabs 130x65 in, regular stock (always orderable)\n",
    ] {
        assert!(system.contains(line), "{system}");
    }
    for hidden in [
        "Hidden Granite",
        "Deleted Granite",
        "Taj Mahal",
        "Other Co Granite",
    ] {
        assert!(!system.contains(hidden), "{system}");
    }
    assert!(
        !system.contains("PRICE LIST"),
        "no price list yet: {system}"
    );
    let prompt = request["messages"][1]["content"].as_str().unwrap();
    assert!(
        prompt.starts_with(
            "CHANNEL: SMS\nSALES REP: Rita Rep, phone 3175550000\nCOMPANY: Acme Stone"
        ),
        "{prompt}"
    );
    assert!(
        prompt.contains("SMS Customer: Can you come measure on Friday?"),
        "{prompt}"
    );
    assert!(
        prompt.contains("/customer/100/stones\nCustomer quote URL (they type their measurements there and send them to us): https://"),
        "{prompt}"
    );
    assert!(prompt.contains("/customer/100/quote\n"), "{prompt}");
    assert!(prompt.ends_with("DRAFT (already typed — continue from its very end):\n\"Fri\"\nThe draft ends mid-word or right after the word \"Fri\". If it is unfinished, complete it (keep \"Fri\", add only the missing letters)."));
    let logged: String =
        sqlx::query_scalar("SELECT feature FROM ai_usage_log ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(logged, "quick_reply");
}

#[test_db::test(seed)]
async fn the_default_price_list_is_in_the_system_message(pool: MySqlPool) {
    let (server, model) = setup(&pool).await;
    sqlx::raw_sql(
        r#"
        INSERT INTO price_lists (id, company_id, name, status, sequence) VALUES
          (900, 100, 'Retail', 'active', 1), (901, 100, 'Builder', 'active', 2);
        INSERT INTO price_list_revisions (id, price_list_id, revision_number, info_json) VALUES
          (910, 900, 1, '{"units": "in", "currency": "$", "expirationDays": 30, "lineItemPriceRounding": 2, "arrOtherItems": [], "arrEdgeProfileSets": [{"arrEdgeProfiles": [{"name": "Ogee", "price": 25}]}],
                         "arrProducts": [{"name": "Granite", "arrVariants": [{"name": "Absolute Black", "price": 80}]}]}'),
          (911, 901, 1, '{"units": "in", "currency": "$", "expirationDays": 30, "lineItemPriceRounding": 2, "arrOtherItems": [], "arrProducts": [], "arrEdgeProfileSets": [{"arrEdgeProfiles": [{"name": "Ogee", "price": 20}]}]}');
        UPDATE price_lists SET current_revision_id = 910 WHERE id = 900;
        UPDATE price_lists SET current_revision_id = 911 WHERE id = 901;
        INSERT INTO quote_settings (company_id, default_price_list_id) VALUES (100, 901);
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    model.reply("");
    server
        .post("/v1/ai/compose/autocomplete")
        .with_session(sessions::REP)
        .json(&json!({ "channel": "sms", "customer_id": 1000, "phone_digits": "3175550100", "draft": "Ogee is" }))
        .await
        .assert_status_ok();
    let system = model.requests()[0]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    // The starred list, not the first one.
    assert!(
        system.ends_with("\n\nPRICE LIST \"Builder\" (what a new quote charges besides the stone):\n- Edges: Ogee $20/lin ft"),
        "{system}"
    );
    assert!(!system.contains("on quotes"), "{system}");
}

#[test_db::test(seed)]
async fn warming_loads_the_thread_without_the_model(pool: MySqlPool) {
    let (server, model) = setup(&pool).await;
    let body = json!({ "channel": "email", "customer_email": " Alice@Example.com ", "draft": "", "warm": true });
    let res = server
        .post("/v1/ai/compose/autocomplete")
        .with_session(sessions::REP)
        .json(&body)
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>(), json!({ "completion": "" }));
    assert!(model.requests().is_empty());

    // An empty draft to someone who never wrote: a whole first message.
    model.reply("Hi! This is Rita with Acme Stone.");
    let res = server
        .post("/v1/ai/compose/autocomplete")
        .with_session(sessions::REP)
        .json(&json!({ "channel": "email", "customer_email": "nobody@example.com", "draft": "" }))
        .await;
    assert_eq!(
        res.json::<Value>()["completion"],
        "Hi! This is Rita with Acme Stone."
    );
    assert_eq!(model.requests()[0]["max_completion_tokens"], 160);
}

#[test_db::test(seed)]
async fn a_composer_needs_someone_to_write_to(pool: MySqlPool) {
    let (server, _) = setup(&pool).await;
    for (body, error) in [
        (
            json!({ "channel": "sms", "draft": "Hi" }),
            "Customer required",
        ),
        (
            json!({ "channel": "sms", "phone_digits": "12ab", "draft": "Hi" }),
            "Invalid phone number",
        ),
    ] {
        let res = server
            .post("/v1/ai/compose/autocomplete")
            .with_session(sessions::REP)
            .json(&body)
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], error);
    }
    server
        .post("/v1/ai/compose/autocomplete")
        .json(&json!({ "channel": "sms", "customer_id": 1000, "draft": "Hi" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

/// Another company's customer id is refused before anything about them is
/// read or sent to the model.
#[test_db::test(seed)]
async fn another_companys_customer_is_not_composed_for(pool: MySqlPool) {
    let (server, model) = setup(&pool).await;
    let res = server
        .post("/v1/ai/compose/autocomplete")
        .with_session(sessions::REP)
        .json(&json!({ "channel": "sms", "customer_id": 2000, "draft": "Hi" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "Customer not found");
    // Not with an own address or phone beside it, nor warming, nor in email.
    for body in [
        json!({ "channel": "sms", "customer_id": 2000, "phone_digits": "3175550100", "draft": "Hi" }),
        json!({ "channel": "email", "customer_id": 2000, "customer_email": "alice@example.com", "draft": "" }),
        json!({ "channel": "sms", "customer_id": 2000, "draft": "", "warm": true }),
        json!({ "channel": "sms", "customer_id": 999_999, "draft": "Hi" }),
    ] {
        let res = server
            .post("/v1/ai/compose/autocomplete")
            .with_session(sessions::REP)
            .json(&body)
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], "Customer not found", "{body}");
    }
    assert!(model.requests().is_empty());

    // Their own rep composes for them as before.
    model.reply("\"there!\"");
    server
        .post("/v1/ai/compose/autocomplete")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "channel": "sms", "customer_id": 2000, "draft": "Hi" }))
        .await
        .assert_status_ok();
    let prompt = model.requests()[0]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(prompt.contains("Other Co Customer"), "{prompt}");
}

/// A cached context is per company: another company's rep asking for the
/// same customer id never gets the one built for its owner.
#[test_db::test(seed)]
async fn a_cached_context_is_never_handed_to_another_company(pool: MySqlPool) {
    let (server, model) = setup(&pool).await;
    model.reply("\"x\"");
    server
        .post("/v1/ai/compose/autocomplete")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "channel": "sms", "customer_id": 2000, "draft": "", "warm": true }))
        .await
        .assert_status_ok();
    let res = server
        .post("/v1/ai/compose/autocomplete")
        .with_session(sessions::REP)
        .json(&json!({ "channel": "sms", "customer_id": 2000, "draft": "Hi" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert!(model.requests().is_empty());
}

#[test_db::test(seed)]
async fn the_inventory_link_is_the_companys_stones_page(pool: MySqlPool) {
    let (server, _) = setup(&pool).await;
    let res = server
        .get("/v1/ai/compose/inventory-url")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    let url = res.json::<Value>()["inventory_url"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(url.ends_with("/customer/100/stones"), "{url}");
}

#[test_db::test(seed)]
async fn what_the_rep_changed_becomes_their_notes(pool: MySqlPool) {
    let (server, model) = setup(&pool).await;
    model.reply(r#"{"notes": ["- Opens with 'Hey', not 'Hi'", "opens with 'hey', not 'hi'"]}"#);
    let res = server
        .post("/v1/ai/compose/learn")
        .with_session(sessions::REP)
        .json(&json!({ "channel": "sms", "customer_id": 1000, "deal_id": 8000,
                       "suggested": "Hi Alice, see you Friday!", "sent": "Hey Alice, see you Friday!" }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>(), json!({ "learned": true }));
    let prompt = model.requests()[0]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(prompt.contains("CURRENT NOTES:\n(none yet)"));
    assert!(prompt.contains(
        "#1 SMS\nAI suggested: Hi Alice, see you Friday!\nRep sent: Hey Alice, see you Friday!"
    ));
    assert_eq!(
        model.requests()[0]["response_format"]["type"],
        "json_object"
    );
    let notes: String = sqlx::query_scalar(
        "SELECT CAST(JSON_EXTRACT(preferences_json, '$.notes') AS CHAR) FROM ai_followup_style_preferences WHERE user_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(notes, r#"["Opens with 'Hey', not 'Hi'"]"#);
    let phase: String =
        sqlx::query_scalar("SELECT phase FROM ai_followup_edit_pairs WHERE user_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(phase, "compose");

    // The next suggestion is written like Rita.
    model.reply(" at 10!");
    server
        .post("/v1/ai/compose/autocomplete")
        .with_session(sessions::REP)
        .json(&json!({ "channel": "sms", "customer_id": 1000, "draft": "See you Friday" }))
        .await
        .assert_status_ok();
    let prompt = model.requests()[1]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(prompt.contains("HOW RITA REP WRITES"), "{prompt}");

    // The same text with other spacing teaches nothing.
    let res = server
        .post("/v1/ai/compose/learn")
        .with_session(sessions::REP)
        .json(&json!({ "channel": "sms", "customer_id": 1000, "suggested": "Hi  Alice", "sent": " Hi Alice " }))
        .await;
    assert_eq!(res.json::<Value>(), json!({ "learned": false }));
    assert_eq!(model.requests().len(), 2);

    // A different message is kept as the rep's voice but rewrites no notes.
    let res = server
        .post("/v1/ai/compose/learn")
        .with_session(sessions::REP)
        .json(&json!({ "channel": "sms", "customer_id": 1000,
                       "suggested": "Hi Alice, I can send you a link to our stones.",
                       "sent": "How would you like to pay the deposit: card or Zelle?" }))
        .await;
    assert_eq!(res.json::<Value>(), json!({ "learned": false }));
    assert_eq!(model.requests().len(), 2);
    let pairs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM ai_followup_edit_pairs WHERE user_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pairs, 2);
}

#[test_db::test(seed)]
async fn a_spouse_writing_from_their_own_address_gets_the_customers_thread(pool: MySqlPool) {
    let (server, model) = setup(&pool).await;
    // Bob, Alice's husband, has no customer of his own: he writes in a
    // thread with Alice copied.
    sqlx::raw_sql(
        "INSERT INTO emails (id, subject, body, sent_at, message_id, thread_id, sender_email, receiver_email, company_id, is_draft)
           VALUES (9901, 'RE: Your new quote', 'What sink did you quote in our plan?', UTC_TIMESTAMP() - INTERVAL 1 HOUR,
                   '<m9901@bob.test>', 'thread-bob', 'bob@work.test', 'rita@acme.test', 100, 0);
         INSERT INTO email_participants (email_id, email, customer_id, user_id, type) VALUES
           (9901, 'bob@work.test', NULL, NULL, 'from'),
           (9901, 'rita@acme.test', 1000, 100, 'to'),
           (9901, 'alice@home.test', 1000, NULL, 'cc');",
    )
    .execute(&pool)
    .await
    .unwrap();
    model.reply("Hi Bob, the plan has our 60/40 sink.");
    server
        .post("/v1/ai/compose/autocomplete")
        .with_session(sessions::REP)
        .json(&json!({ "channel": "email", "customer_email": "Bob@work.test", "draft": "" }))
        .await
        .assert_status_ok();
    let prompt = model.requests()[0]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        prompt.contains(
            "EMAIL Customer: Subject: RE: Your new quote\nWhat sink did you quote in our plan?"
        ),
        "{prompt}"
    );
    assert!(
        prompt.contains("SMS Customer: Can you come measure on Friday?"),
        "{prompt}"
    );
    assert!(
        prompt.ends_with("DRAFT: (empty — suggest how the next message starts)"),
        "{prompt}"
    );
}
