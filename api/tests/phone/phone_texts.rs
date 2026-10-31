//! The phone app's Texts tab (`/v1/phone/texts*`, device token), the
//! bootstrap's `texting`, and the `text` push for an inbound text.
//!
//! Acme (100) texts through Telnyx (messaging profile `profile-100`). Rita
//! (100, phone 1) has her direct line 317-555-0151 (number 9701) and, with
//! Mark (101, phone 2), the shared line 317-555-0152 (9702); 317-555-0153
//! (9703) is Ada's (102, phone 3). Alice (1000) is 317-555-0100, Bob (1001)
//! 317-555-0102, Carol (1002) 317-555-0103.

use crate::common;
use crate::phone_common;

use api::telnyx::Telnyx;
use axum::Json;
use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum_test::TestServer;
use common::{WithSession, sessions};
use phone_common::{
    HEADER, Stubs, add_number, add_phone, assign, enable_calling, phone_server_with, token,
    webhook_token,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

const KEY: &str = "texts-telnyx-key";
/// Telnyx refuses texts to this phone (it texted STOP elsewhere): 40300.
const BLOCKED: &str = "+13175550666";

/// Telnyx's `POST /messages` (a stand-in apart from `phone_common`'s, which
/// keeps FCM).
#[derive(Clone, Default)]
struct Messages {
    sent: Arc<Mutex<Vec<Value>>>,
}

impl Messages {
    fn sent(&self) -> Vec<Value> {
        self.sent.lock().unwrap().clone()
    }

    async fn answer(&self, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        if parts.uri.path() != "/messages" {
            return StatusCode::NOT_FOUND.into_response();
        }
        assert_eq!(parts.headers["authorization"], format!("Bearer {KEY}"));
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        if body["to"] == BLOCKED {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "errors": [{ "code": "40300", "title": "Blocked due to STOP message" }] })),
            )
                .into_response();
        }
        let n = {
            let mut sent = self.sent.lock().unwrap();
            sent.push(body);
            sent.len()
        };
        Json(json!({ "data": { "id": format!("msg-{n}"), "to": [{ "status": "queued" }] } }))
            .into_response()
    }
}

struct Setup {
    server: TestServer,
    stubs: Stubs,
    messages: Messages,
}

async fn setup(pool: &MySqlPool, texting: bool) -> Setup {
    let extra = if texting {
        "messaging_profile_id = 'profile-100'"
    } else {
        ""
    };
    enable_calling(pool, 100, extra).await;
    add_number(pool, 9701, 100, "+13175550151", "direct", "Rita").await;
    add_number(pool, 9702, 100, "+13175550152", "shared", "Showroom").await;
    add_number(pool, 9703, 100, "+13175550153", "direct", "Ada").await;
    assign(pool, 9701, 100, true, true).await;
    assign(pool, 9702, 100, true, true).await;
    assign(pool, 9702, 101, true, true).await;
    assign(pool, 9703, 102, true, true).await;
    add_phone(pool, 1, 100, 100).await;
    add_phone(pool, 2, 101, 100).await;
    add_phone(pool, 3, 102, 100).await;
    sqlx::query("UPDATE customers SET company_name = 'Alice Homes' WHERE id = 1000")
        .execute(pool)
        .await
        .unwrap();
    let messages = Messages::default();
    let handler = messages.clone();
    let router = Router::new().fallback(move |request: Request| {
        let handler = handler.clone();
        async move { handler.answer(request).await }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{address}");
    let (server, stubs) = phone_server_with(pool.clone(), move |state| {
        state.with_telnyx(Telnyx::new(Some(KEY), &base))
    })
    .await;
    Setup {
        server,
        stubs,
        messages,
    }
}

impl Setup {
    /// An inbound text Telnyx reports (unsigned: the tests' config takes them).
    async fn inbound(&self, id: &str, from: &str, to: &str, text: &str, media: Value) {
        let event = json!({
            "data": { "event_type": "message.received", "id": format!("ev-{id}"),
              "payload": { "id": id, "direction": "inbound", "text": text,
                "from": { "phone_number": from }, "to": [{ "phone_number": to }],
                "media": media, "messaging_profile_id": "profile-100" } }
        });
        self.server
            .post(&format!(
                "/v1/webhooks/telnyx/{}/messaging",
                webhook_token(100)
            ))
            .json(&event)
            .await
            .assert_status_ok();
    }

    async fn get(&self, phone: u32, path: &str) -> axum_test::TestResponse {
        self.server.get(path).add_header(HEADER, token(phone)).await
    }

    async fn json(&self, phone: u32, path: &str) -> Value {
        let res = self.get(phone, path).await;
        res.assert_status_ok();
        res.json()
    }

    async fn send(&self, phone: u32, body: Value) -> axum_test::TestResponse {
        self.server
            .post("/v1/phone/texts")
            .add_header(HEADER, token(phone))
            .json(&body)
            .await
    }

    /// Alice texted Rita's line, Bob the shared line, Carol Ada's line.
    async fn three_threads(&self) {
        self.inbound("in-a", "+13175550100", "+13175550151", "Hi Rita", json!([]))
            .await;
        self.inbound(
            "in-b",
            "+13175550102",
            "+13175550152",
            "Hi showroom",
            json!([]),
        )
        .await;
        self.inbound("in-c", "+13175550103", "+13175550153", "Hi Ada", json!([]))
            .await;
    }
}

fn keys(list: &Value) -> Vec<&str> {
    list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["key"].as_str().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn the_texts_tab_needs_texting_on(pool: MySqlPool) {
    let setup = setup(&pool, false).await;
    let res = setup.get(1, "/v1/phone/texts/threads").await;
    res.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(res.json::<Value>()["error"], "texting_disabled");
    let res = setup
        .send(
            1,
            json!({ "ourNumberId": 9701, "to": "+13175550100", "text": "Hi" }),
        )
        .await;
    res.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(res.json::<Value>()["error"], "texting_disabled");
    let boot = setup.json(1, "/v1/phone/bootstrap").await;
    assert_eq!(boot["texting"], json!({ "enabled": false, "unread": 0 }));
    // The CRM cookie is no device token.
    setup
        .server
        .get("/v1/phone/texts/threads")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn my_threads_are_those_of_my_lines(pool: MySqlPool) {
    let setup = setup(&pool, true).await;
    setup.three_threads().await;

    let rita = setup.json(1, "/v1/phone/texts/threads").await;
    assert_eq!(keys(&rita), ["9702-3175550102", "9701-3175550100"]);
    assert_eq!(rita["unread"], 2);
    assert_eq!(rita["nextBefore"], Value::Null);
    let alice = &rita["threads"][1];
    assert_eq!(
        alice["ourNumber"],
        json!({ "id": 9701, "e164": "+13175550151", "label": "Rita" })
    );
    assert_eq!(alice["otherNumber"], "+13175550100");
    assert_eq!(
        alice["customer"],
        json!({ "id": 1000, "name": "Alice Lead", "company": "Alice Homes" })
    );
    assert_eq!(alice["last"]["direction"], "inbound");
    assert_eq!(alice["last"]["text"], "Hi Rita");
    assert_eq!(alice["last"]["hasMedia"], false);
    assert_eq!(alice["last"]["status"], "received");
    assert_eq!(alice["unread"], 1);

    // Paging, and one other number's threads.
    let first = setup.json(1, "/v1/phone/texts/threads?limit=1").await;
    assert_eq!(keys(&first), ["9702-3175550102"]);
    let before = first["nextBefore"].as_str().unwrap().to_string();
    let second = setup
        .json(
            1,
            &format!("/v1/phone/texts/threads?limit=1&before={before}"),
        )
        .await;
    assert_eq!(keys(&second), ["9701-3175550100"]);
    assert_eq!(second["nextBefore"], Value::Null);
    let only = setup
        .json(1, "/v1/phone/texts/threads?number=%2B13175550100")
        .await;
    assert_eq!(keys(&only), ["9701-3175550100"]);

    // Mark shares only the showroom line; Carol's thread is Ada's.
    let mark = setup.json(2, "/v1/phone/texts/threads").await;
    assert_eq!(keys(&mark), ["9702-3175550102"]);
    setup
        .get(1, "/v1/phone/texts/threads/9703-3175550103/messages")
        .await
        .assert_status(StatusCode::NOT_FOUND);
    setup
        .get(1, "/v1/phone/texts/threads/junk/messages")
        .await
        .assert_status(StatusCode::NOT_FOUND);

    let thread = setup
        .json(1, "/v1/phone/texts/threads/9701-3175550100/messages")
        .await;
    assert_eq!(thread["thread"]["key"], "9701-3175550100");
    let message = &thread["messages"][0];
    assert_eq!(message["direction"], "inbound");
    assert_eq!(message["text"], "Hi Rita");
    assert_eq!(message["status"], "received");
    assert_eq!(message["error"], Value::Null);
    assert_eq!(message["sentBy"], Value::Null);
    assert_eq!(message["media"], json!([]));

    // The bootstrap's badge.
    let boot = setup.json(1, "/v1/phone/bootstrap").await;
    assert_eq!(boot["texting"], json!({ "enabled": true, "unread": 2 }));
}

#[test_db::test(seed)]
async fn an_inbound_text_pushes_the_lines_people(pool: MySqlPool) {
    let setup = setup(&pool, true).await;
    setup
        .inbound(
            "in-b",
            "+13175550102",
            "+13175550152",
            "Hi showroom",
            json!([]),
        )
        .await;
    let mut pushes: Vec<(String, Value)> = setup
        .stubs
        .pushes()
        .into_iter()
        .filter(|push| push["data"]["type"] == "text")
        .map(|push| {
            (
                push["token"].as_str().unwrap().to_string(),
                push["data"].clone(),
            )
        })
        .collect();
    pushes.sort_by(|a, b| a.0.cmp(&b.0));
    // The push names the text only: no number, not even in a thread key.
    let text_id: i64 =
        sqlx::query_scalar("SELECT CAST(id AS SIGNED) FROM telnyx_sms WHERE telnyx_id = 'in-b'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let data = json!({ "granite": "1", "type": "text", "textId": text_id.to_string() });
    assert_eq!(
        pushes,
        [
            ("push-1".to_string(), data.clone()),
            ("push-2".to_string(), data)
        ]
    );
    assert!(!pushes[0].1.to_string().contains("3175550102"));
    // The phones get the thread from the text's id; not Ada's (not her line).
    let path = format!("/v1/phone/texts/messages/{text_id}/thread");
    for phone in [1, 2] {
        let thread = setup.json(phone, &path).await;
        assert_eq!(thread["key"], "9702-3175550102");
        assert_eq!(thread["otherNumber"], "+13175550102");
        assert_eq!(thread["customer"]["name"], "Bob Unassigned");
        assert_eq!(thread["unread"], 1);
    }
    for (phone, path) in [
        (3, path.clone()),
        (1, "/v1/phone/texts/messages/999999/thread".to_string()),
        (1, "/v1/phone/texts/messages/abc/thread".to_string()),
    ] {
        setup
            .get(phone, &path)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    setup
        .server
        .get(&path)
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // Telnyx sends it again: no second push.
    setup
        .inbound(
            "in-b",
            "+13175550102",
            "+13175550152",
            "Hi showroom",
            json!([]),
        )
        .await;
    assert_eq!(
        setup
            .stubs
            .pushes()
            .iter()
            .filter(|push| push["data"]["type"] == "text")
            .count(),
        2
    );
}

#[test_db::test(seed)]
async fn i_text_from_my_lines(pool: MySqlPool) {
    let setup = setup(&pool, true).await;
    let res = setup
        .send(
            1,
            json!({ "ourNumberId": 9701, "to": "3175550100", "text": "  On my way  " }),
        )
        .await;
    res.assert_status(StatusCode::CREATED);
    let sent: Value = res.json();
    assert_eq!(sent["direction"], "outbound");
    assert_eq!(sent["text"], "On my way");
    assert_eq!(sent["status"], "queued");
    assert_eq!(sent["sentBy"], "Rita Rep");
    assert_eq!(setup.messages.sent()[0]["from"], "+13175550151");
    assert_eq!(setup.messages.sent()[0]["to"], "+13175550100");

    let thread = setup
        .json(1, "/v1/phone/texts/threads/9701-3175550100/messages")
        .await;
    assert_eq!(thread["messages"][0]["id"], sent["id"]);
    assert_eq!(thread["thread"]["last"]["status"], "queued");

    // Not my line, not a number, an opted-out phone, Telnyx's refusal.
    let refused = |phone: u32, body: Value| {
        let setup = &setup;
        async move { setup.send(phone, body).await }
    };
    let res = refused(
        1,
        json!({ "ourNumberId": 9703, "to": "3175550100", "text": "Hi" }),
    )
    .await;
    res.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(res.json::<Value>()["error"], "not_my_line");
    let res = refused(
        1,
        json!({ "ourNumberId": 9701, "to": "+443069990000", "text": "Hi" }),
    )
    .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "invalid_phone");
    let res = refused(
        1,
        json!({ "ourNumberId": 9701, "to": "3175550100", "text": "  " }),
    )
    .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    sqlx::query(
        "INSERT INTO sms_opt_outs (company_id, phone10, source, opted_out_at)
         VALUES (100, '3175550102', 'stop', UTC_TIMESTAMP())",
    )
    .execute(&pool)
    .await
    .unwrap();
    let res = refused(
        1,
        json!({ "ourNumberId": 9702, "to": "3175550102", "text": "Hi" }),
    )
    .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(res.json::<Value>()["error"], "opted_out");
    let res = refused(
        1,
        json!({ "ourNumberId": 9701, "to": BLOCKED, "text": "Hi" }),
    )
    .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(res.json::<Value>()["error"], "opted_out");
    assert_eq!(setup.messages.sent().len(), 1);

    // Ten texts a minute (shared with the CRM's composer).
    // Two texts went to Telnyx so far (one sent, one refused): eight more fill the minute.
    for n in 0..8 {
        setup
            .send(
                1,
                json!({ "ourNumberId": 9701, "to": "3175550100", "text": format!("#{n}") }),
            )
            .await
            .assert_status(StatusCode::CREATED);
    }
    let res = setup
        .send(
            1,
            json!({ "ourNumberId": 9701, "to": "3175550100", "text": "one more" }),
        )
        .await;
    res.assert_status(StatusCode::TOO_MANY_REQUESTS);
    let body: Value = res.json();
    assert_eq!(body["error"], "rate_limited");
    assert!(body["retryAfterSecs"].as_u64().unwrap() >= 1);
    assert!(res.headers().contains_key("retry-after"));
}

#[test_db::test(seed)]
async fn reading_a_thread_clears_my_unread_here_and_in_the_crm(pool: MySqlPool) {
    let setup = setup(&pool, true).await;
    setup.three_threads().await;
    setup
        .server
        .post("/v1/phone/texts/threads/9702-3175550102/read")
        .add_header(HEADER, token(2))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let mark = setup.json(2, "/v1/phone/texts/threads").await;
    assert_eq!(mark["unread"], 0);
    assert_eq!(mark["threads"][0]["unread"], 0);
    let crm = setup
        .server
        .get("/v1/sms/telnyx/unread-count")
        .with_session(sessions::MANAGER)
        .await;
    crm.assert_status_ok();
    assert_eq!(crm.json::<Value>()["count"], 0);
    // Rita's own marker is hers.
    let rita = setup.json(1, "/v1/phone/texts/threads").await;
    assert_eq!(rita["unread"], 2);
    // The Unread tab: only my unread threads, paged like the rest.
    let unread = setup.json(1, "/v1/phone/texts/threads?unread=true").await;
    assert_eq!(keys(&unread), ["9702-3175550102", "9701-3175550100"]);
    setup
        .server
        .post("/v1/phone/texts/threads/9702-3175550102/read")
        .add_header(HEADER, token(1))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let unread = setup
        .json(1, "/v1/phone/texts/threads?unread=true&limit=1")
        .await;
    assert_eq!(keys(&unread), ["9701-3175550100"]);
    assert_eq!(unread["nextBefore"], Value::Null);
    assert_eq!(unread["unread"], 1);
    let mark = setup.json(2, "/v1/phone/texts/threads?unread=true").await;
    assert_eq!(keys(&mark), Vec::<&str>::new());
    // Not my line.
    setup
        .server
        .post("/v1/phone/texts/threads/9703-3175550103/read")
        .add_header(HEADER, token(2))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn pictures_come_as_short_links(pool: MySqlPool) {
    let setup = setup(&pool, true).await;
    let url = setup.stubs.add_file("slab.png", &common::png(4, 3));
    setup
        .inbound(
            "in-mms",
            "+13175550100",
            "+13175550151",
            "",
            json!([{ "url": url, "content_type": "image/png", "size": 80 }]),
        )
        .await;
    let rita = setup.json(1, "/v1/phone/texts/threads").await;
    assert_eq!(rita["threads"][0]["last"]["hasMedia"], true);
    let thread = setup
        .json(
            1,
            "/v1/phone/texts/threads/9701-3175550100/messages?limit=1",
        )
        .await;
    let media = &thread["messages"][0]["media"][0];
    assert_eq!(media["contentType"], "image/png");
    let id = media["id"].as_str().unwrap().to_string();
    let link = setup.json(1, &format!("/v1/phone/texts/media/{id}")).await;
    assert!(
        link["url"]
            .as_str()
            .unwrap()
            .contains("sms-attachments/100/tx-")
    );
    assert!(link["expiresAt"].as_str().is_some());
    // Ada may not see Rita's line's files.
    setup
        .get(3, &format!("/v1/phone/texts/media/{id}"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn texts_from_my_earlier_provider_are_on_my_line(pool: MySqlPool) {
    let setup = setup(&pool, true).await;
    // Rita texted Dave (317-555-0104) through RingCentral before Granite Phone.
    sqlx::raw_sql(
        "UPDATE users SET ringcentral_extension_id = '777', ringcentral_phone_number = '3175550199'
          WHERE id = 100;
         INSERT INTO ringcentral_sms
           (sender, recipient, text, direction, status, agent, company_id, created_date,
            sender10, recipient10, phone_digits)
         VALUES (13175550199, 13175550104, 'Your slab is in', 'outbound', 'sent', '777', 100,
                 '2026-01-05 10:00:00', '3175550199', '3175550104', '3175550104'),
                (13175550104, 13175550199, 'Great, thanks', 'inbound', 'received', NULL, 100,
                 '2026-01-05 11:00:00', '3175550104', '3175550199', '3175550104');",
    )
    .execute(&pool)
    .await
    .unwrap();
    api::routes::sms::moves::carry(&pool, 100, 100)
        .await
        .unwrap();

    // On her own line (not the shared one), in the app.
    let rita = setup
        .json(1, "/v1/phone/texts/threads?number=%2B13175550104")
        .await;
    assert_eq!(keys(&rita), ["9701-3175550104"]);
    let thread = setup
        .json(1, "/v1/phone/texts/threads/9701-3175550104/messages")
        .await;
    let texts: Vec<&str> = thread["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["Great, thanks", "Your slab is in"]);
    // Mark, on the shared line with her, does not get her history.
    let mark = setup
        .json(2, "/v1/phone/texts/threads?number=%2B13175550104")
        .await;
    assert_eq!(keys(&mark), Vec::<&str>::new());
}
