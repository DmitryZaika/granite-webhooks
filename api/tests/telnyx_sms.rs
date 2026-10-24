//! Texting through Telnyx (`/v1/sms/telnyx/...`, `POST .../messaging`): the
//! signed messaging webhook (inbound texts and MMS, delivery status, STOP),
//! sending SMS and MMS, which provider a user texts through, a drip text
//! going out through Telnyx, and the inbox routes.
//!
//! Acme (100) texts through Telnyx (messaging profile `profile-100`): Rita
//! (100) has her direct line 317-555-0151 (number 9701) and, with Mark
//! (101), the shared line 317-555-0152 (9702); 317-555-0153 (9703) is Ada's
//! (102). Alice (1000) is 317-555-0100; Bob (1001) is 317-555-0102 with deal
//! 8001 on the first list ("New").

mod common;
mod phone_common;

use api::app;
use api::state::AppState;
use api::storage::Storage;
use api::telnyx::Telnyx;
use axum::Json;
use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use phone_common::{Signer, add_number, assign, enable_calling, signed_config, webhook_token};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const KEY: &str = "test-telnyx-key";
/// Telnyx refuses texts to this phone: it texted STOP (error 40300).
const BLOCKED: &str = "+13175550666";

/// A stand-in for Telnyx: `POST /telnyx/messages` and media files at
/// `/media/{name}`.
#[derive(Clone, Default)]
struct Stub {
    messages: Arc<Mutex<Vec<Value>>>,
    sent: Arc<AtomicUsize>,
    base: Arc<Mutex<String>>,
}

impl Stub {
    fn messages(&self) -> Vec<Value> {
        self.messages.lock().unwrap().clone()
    }

    fn base(&self) -> String {
        self.base.lock().unwrap().clone()
    }

    async fn answer(&self, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        let path = parts.uri.path().to_string();
        if path == "/telnyx/messages" {
            assert_eq!(parts.headers["authorization"], format!("Bearer {KEY}"));
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            self.messages.lock().unwrap().push(body.clone());
            if body["to"] == BLOCKED {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "errors": [{ "code": "40300", "title": "Blocked due to STOP message" }] })),
                )
                    .into_response();
            }
            let n = self.sent.fetch_add(1, Ordering::SeqCst) + 1;
            return Json(
                json!({ "data": { "id": format!("msg-{n}"), "record_type": "message",
                "to": [{ "phone_number": body["to"], "status": "queued" }] } }),
            )
            .into_response();
        }
        if path.starts_with("/media/") {
            return ([(CONTENT_TYPE, "image/png")], common::png(4, 3)).into_response();
        }
        StatusCode::NOT_FOUND.into_response()
    }
}

struct Setup {
    server: TestServer,
    stub: Stub,
    signer: Signer,
    storage: Storage,
    state: AppState,
}

const FIXTURE: &str = "
UPDATE telnyx_company_config SET messaging_profile_id = 'profile-100' WHERE company_id = 100;
INSERT INTO sms_flows (id, company_id, user_id, name) VALUES (9600, 100, 100, 'Nurture');
INSERT INTO sms_flow_enrollments (id, company_id, flow_id, user_id, customer_id, customer_phone_digits,
                                  status, current_step_position, anchor_at, next_send_at, started_at, updated_at)
  VALUES (9601, 100, 9600, 100, 1000, '3175550100', 'active', 1, UTC_TIMESTAMP(), '2099-01-01 00:00:00',
          UTC_TIMESTAMP(), UTC_TIMESTAMP());
";

async fn setup(pool: &MySqlPool) -> Setup {
    enable_calling(pool, 100, "").await;
    add_number(pool, 9701, 100, "+13175550151", "direct", "Rita").await;
    add_number(pool, 9702, 100, "+13175550152", "shared", "Showroom").await;
    add_number(pool, 9703, 100, "+13175550153", "direct", "Ada").await;
    assign(pool, 9701, 100, true, true).await;
    assign(pool, 9702, 100, true, true).await;
    assign(pool, 9702, 101, true, true).await;
    assign(pool, 9703, 102, true, true).await;
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    let stub = Stub::default();
    let handler = stub.clone();
    let router = Router::new().fallback(move |request: Request| {
        let handler = handler.clone();
        async move { handler.answer(request).await }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    *stub.base.lock().unwrap() = format!("http://{address}");
    let signer = Signer::new();
    let storage = Storage::memory();
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_telnyx(Telnyx::new(Some(KEY), &format!("http://{address}/telnyx")))
        .with_storage(storage.clone())
        .with_job_key("job-secret")
        .with_phone_config(signed_config(&signer.public_key));
    Setup {
        server: TestServer::new(app(state.clone())),
        stub,
        signer,
        storage,
        state,
    }
}

fn hook_path() -> String {
    format!("/v1/webhooks/telnyx/{}/messaging", webhook_token(100))
}

impl Setup {
    /// Posts a messaging event signed like Telnyx does.
    async fn hook(&self, event: &Value) -> axum_test::TestResponse {
        let body = serde_json::to_vec(event).unwrap();
        let [(sig_name, sig), (ts_name, ts)] = self.signer.headers(&body);
        self.server
            .post(&hook_path())
            .add_header(sig_name, sig)
            .add_header(ts_name, ts)
            .content_type("application/json")
            .bytes(body.into())
            .await
    }

    async fn send(&self, session: &str, body: Value) -> axum_test::TestResponse {
        self.server
            .post("/v1/sms/telnyx/messages")
            .with_session(session)
            .json(&body)
            .await
    }

    async fn get(&self, path: &str, session: &str) -> Value {
        let res = self.server.get(path).with_session(session).await;
        res.assert_status_ok();
        res.json()
    }
}

fn inbound(id: &str, from: &str, to: &str, text: &str, media: Value) -> Value {
    json!({
        "data": { "event_type": "message.received", "id": format!("ev-{id}"),
          "occurred_at": "2026-10-22T15:00:00.000+00:00", "record_type": "event",
          "payload": { "id": id, "direction": "inbound", "type": "SMS", "text": text,
            "from": { "phone_number": from, "carrier": "T-Mobile USA" },
            "to": [{ "phone_number": to, "status": "webhook_delivered" }],
            "media": media, "messaging_profile_id": "profile-100" } },
        "meta": { "attempt": 1 }
    })
}

fn status(event: &str, id: &str, status: &str, errors: Value) -> Value {
    json!({
        "data": { "event_type": event, "id": format!("ev-{id}-{status}"),
          "occurred_at": chrono::Utc::now().to_rfc3339(),
          "payload": { "id": id, "direction": "outbound",
            "to": [{ "phone_number": "+13175550100", "status": status }],
            "errors": errors, "messaging_profile_id": "profile-100" } }
    })
}

#[derive(sqlx::FromRow, Debug)]
struct Row {
    id: i32,
    telnyx_id: Option<String>,
    direction: String,
    status: String,
    text: String,
    sender10: Option<String>,
    recipient10: Option<String>,
    phone_digits: Option<String>,
    agent: Option<String>,
    number_id: Option<i32>,
    delivery_status: Option<String>,
    error_code: Option<String>,
    is_automated: bool,
}

async fn rows(pool: &MySqlPool) -> Vec<Row> {
    sqlx::query_as(
        "SELECT id, telnyx_id, CAST(direction AS CHAR) AS direction, CAST(status AS CHAR) AS status,
                text, sender10, recipient10, phone_digits, agent, number_id, delivery_status,
                error_code, is_automated <> 0 AS is_automated
           FROM telnyx_sms ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn opted_out(pool: &MySqlPool, phone10: &str) -> Option<String> {
    sqlx::query_scalar("SELECT source FROM sms_opt_outs WHERE company_id = 100 AND phone10 = ?")
        .bind(phone10)
        .fetch_optional(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn the_messaging_webhook_needs_telnyx_signature(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let event = inbound("in-1", "+13175550100", "+13175550151", "Hi", json!([]));
    let body = serde_json::to_vec(&event).unwrap();

    // No signature, and a forged one: refused, nothing stored.
    let unsigned = setup.server.post(&hook_path()).json(&event).await;
    unsigned.assert_status(StatusCode::UNAUTHORIZED);
    let forger = Signer::new();
    let [(sig_name, sig), (ts_name, ts)] = forger.headers(&body);
    let forged = setup
        .server
        .post(&hook_path())
        .add_header(sig_name, sig)
        .add_header(ts_name, ts)
        .content_type("application/json")
        .bytes(body.clone().into())
        .await;
    forged.assert_status(StatusCode::UNAUTHORIZED);
    // Signed for another body.
    let [(sig_name, sig), (ts_name, ts)] = setup.signer.headers(b"{}");
    let mangled = setup
        .server
        .post(&hook_path())
        .add_header(sig_name, sig)
        .add_header(ts_name, ts)
        .content_type("application/json")
        .bytes(body.into())
        .await;
    mangled.assert_status(StatusCode::UNAUTHORIZED);
    assert!(rows(&pool).await.is_empty());

    // An unknown token, and another company's messaging profile.
    let unknown = setup
        .server
        .post("/v1/webhooks/telnyx/nobody/messaging")
        .json(&event)
        .await;
    unknown.assert_status(StatusCode::NOT_FOUND);
    let mut other = event.clone();
    other["data"]["payload"]["messaging_profile_id"] = json!("profile-999");
    setup
        .hook(&other)
        .await
        .assert_status(StatusCode::FORBIDDEN);

    setup.hook(&event).await.assert_status_ok();
    assert_eq!(rows(&pool).await.len(), 1);
}

#[test_db::test(seed)]
async fn an_inbound_text_is_stored_once_and_stops_flows_and_moves_the_deal(pool: MySqlPool) {
    let setup = setup(&pool).await;
    sqlx::query(
        "INSERT INTO sms_flow_enrollments (id, company_id, flow_id, user_id, customer_id, customer_phone_digits,
                                           status, current_step_position, anchor_at, next_send_at, started_at, updated_at)
         VALUES (9602, 100, 9600, 100, 1001, '3175550102', 'active', 1, UTC_TIMESTAMP(), '2099-01-01 00:00:00',
                 UTC_TIMESTAMP(), UTC_TIMESTAMP())",
    )
    .execute(&pool)
    .await
    .unwrap();
    let event = inbound(
        "in-bob",
        "+13175550102",
        "+13175550152",
        "Is the slab still available?",
        json!([]),
    );
    setup.hook(&event).await.assert_status_ok();
    // Telnyx sends it again: nothing more happens.
    setup.hook(&event).await.assert_status_ok();

    let stored = rows(&pool).await;
    assert_eq!(stored.len(), 1);
    let row = &stored[0];
    assert_eq!(row.telnyx_id.as_deref(), Some("in-bob"));
    assert_eq!(
        (row.direction.as_str(), row.status.as_str()),
        ("inbound", "received")
    );
    assert_eq!(row.text, "Is the slab still available?");
    assert_eq!(row.sender10.as_deref(), Some("3175550102"));
    assert_eq!(row.recipient10.as_deref(), Some("3175550152"));
    assert_eq!(row.phone_digits.as_deref(), Some("3175550102"));
    assert_eq!((row.agent.as_deref(), row.number_id), (None, Some(9702)));

    let flow: String =
        sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM sms_flow_enrollments WHERE id = 9602")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(flow, "stopped_by_reply");
    let alice_flow: String =
        sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM sms_flow_enrollments WHERE id = 9601")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(alice_flow, "active", "another customer's flow keeps going");
    let list: i32 = sqlx::query_scalar("SELECT list_id FROM deals WHERE id = 8001")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(list, 101, "Bob's deal left the first stage");

    // A text to a number that is not the company's is ignored.
    setup
        .hook(&inbound(
            "in-x",
            "+13175550102",
            "+13175550999",
            "hello",
            json!([]),
        ))
        .await
        .assert_status_ok();
    assert_eq!(rows(&pool).await.len(), 1);
}

#[test_db::test(seed)]
async fn an_inbound_mms_keeps_its_files(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let media = json!([{ "url": format!("{}/media/slab.png", setup.stub.base()),
                         "content_type": "image/png", "size": 70 }]);
    setup
        .hook(&inbound(
            "in-mms",
            "+13175550100",
            "+13175550151",
            "",
            media,
        ))
        .await
        .assert_status_ok();
    let stored = rows(&pool).await;
    assert_eq!(stored.len(), 1);
    let files: Vec<(String, String, Option<i32>, Option<i32>)> = sqlx::query_as(
        "SELECT content_type, s3_key, width, height FROM telnyx_sms_attachments WHERE telnyx_sms_id = ?",
    )
    .bind(stored[0].id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, "image/png");
    assert!(
        files[0]
            .1
            .starts_with(&format!("sms-attachments/100/tx-{}/", stored[0].id))
    );
    assert_eq!((files[0].2, files[0].3), (Some(4), Some(3)));
    assert!(setup.storage.memory_keys().contains(&files[0].1));

    // The thread shows the picture.
    let thread = setup
        .get("/v1/sms/telnyx/threads/3175550100", sessions::REP)
        .await;
    let message = &thread["messages"][0];
    assert_eq!(message["direction"], "inbound");
    assert_eq!(message["attachments"][0]["content_type"], "image/png");
    assert_eq!(message["agent_name"], "Rita Rep", "her direct line got it");
}

#[test_db::test(seed)]
async fn stop_opts_a_phone_out_and_start_opts_it_back_in(pool: MySqlPool) {
    let setup = setup(&pool).await;
    setup
        .hook(&inbound(
            "in-stop",
            "+13175550100",
            "+13175550151",
            " Stop. ",
            json!([]),
        ))
        .await
        .assert_status_ok();
    assert_eq!(
        opted_out(&pool, "3175550100").await.as_deref(),
        Some("stop")
    );

    // Texts to it are refused before Telnyx is called.
    let res = setup
        .send(
            sessions::REP,
            json!({ "phone_digits": "3175550100", "text": "Hello?" }),
        )
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "opted_out");
    assert!(setup.stub.messages().is_empty());

    setup
        .hook(&inbound(
            "in-start",
            "+13175550100",
            "+13175550151",
            "START",
            json!([]),
        ))
        .await
        .assert_status_ok();
    assert_eq!(opted_out(&pool, "3175550100").await, None);
    setup
        .send(
            sessions::REP,
            json!({ "phone_digits": "3175550100", "text": "Welcome back" }),
        )
        .await
        .assert_status_ok();
    assert_eq!(setup.stub.messages().len(), 1);

    // Telnyx refuses a phone that texted STOP elsewhere on the profile (40300).
    let res = setup
        .send(
            sessions::REP,
            json!({ "phone_digits": "3175550666", "text": "Hi" }),
        )
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "opted_out");
    assert_eq!(
        opted_out(&pool, "3175550666").await.as_deref(),
        Some("carrier")
    );
    let failed = rows(&pool).await.into_iter().last().unwrap();
    assert_eq!(
        (failed.status.as_str(), failed.error_code.as_deref()),
        ("failed", Some("40300"))
    );

    // The FCC's other opt-out words count as STOP too.
    for (id, from, text) in [
        ("in-opt-out", "+13175550101", "Opt out"),
        ("in-revoke", "+13175550102", "REVOKE."),
    ] {
        setup
            .hook(&inbound(id, from, "+13175550151", text, json!([])))
            .await
            .assert_status_ok();
    }
    for phone in ["3175550101", "3175550102"] {
        assert_eq!(opted_out(&pool, phone).await.as_deref(), Some("stop"));
    }
}

#[test_db::test(seed)]
async fn a_text_goes_out_from_the_users_line(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let res = setup
        .send(
            sessions::REP,
            json!({ "phone_digits": "3175550100", "text": "  Your quote is ready  " }),
        )
        .await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["delayed"], false);
    assert_eq!(body["message"]["provider"], "telnyx");
    assert_eq!(body["message"]["direction"], "outbound");
    assert_eq!(body["message"]["text"], "Your quote is ready");
    assert_eq!(body["message"]["agent_id"], "100");
    assert_eq!(body["message"]["agent_name"], "Rita Rep");

    assert_eq!(
        setup.stub.messages(),
        [json!({
            "from": "+13175550151", "to": "+13175550100", "text": "Your quote is ready",
            "messaging_profile_id": "profile-100",
            "webhook_url": format!("https://api.test/v1/webhooks/telnyx/{}/messaging", webhook_token(100)),
        })]
    );
    let row = rows(&pool).await.remove(0);
    assert_eq!(row.telnyx_id.as_deref(), Some("msg-1"));
    assert_eq!(
        (row.status.as_str(), row.delivery_status.as_deref()),
        ("sent", Some("queued"))
    );
    assert_eq!(
        (
            row.sender10.as_deref(),
            row.phone_digits.as_deref(),
            row.agent.as_deref()
        ),
        (Some("3175550151"), Some("3175550100"), Some("100"))
    );
    assert!(!row.is_automated);
    let read: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM telnyx_sms_thread_reads WHERE user_id = 100 AND customer_phone_digits = '3175550100'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(read, 1);

    // Telnyx reports it delivered, then a later, older "sent" changes nothing.
    setup
        .hook(&status(
            "message.finalized",
            "msg-1",
            "delivered",
            json!([]),
        ))
        .await
        .assert_status_ok();
    setup
        .hook(&status("message.sent", "msg-1", "sent", json!([])))
        .await
        .assert_status_ok();
    let row = rows(&pool).await.remove(0);
    assert_eq!(
        (row.status.as_str(), row.delivery_status.as_deref()),
        ("sent", Some("delivered"))
    );

    // Mark has only the shared line: that is his line.
    setup
        .send(
            sessions::MANAGER,
            json!({ "phone_digits": "3175550102", "text": "Hi Bob" }),
        )
        .await
        .assert_status_ok();
    assert_eq!(setup.stub.messages()[1]["from"], "+13175550152");

    // Vic has no line: refused; outside the US and Canada: refused.
    let res = setup
        .send(
            sessions::VIEWER,
            json!({ "phone_digits": "3175550100", "text": "Hi" }),
        )
        .await;
    assert_ne!(res.status_code(), StatusCode::OK);
    let res = setup
        .send(
            sessions::REP,
            json!({ "phone_digits": "443069990000", "text": "Hi" }),
        )
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "invalid_phone");
}

#[test_db::test(seed)]
async fn a_failed_delivery_marks_the_text_failed(pool: MySqlPool) {
    let setup = setup(&pool).await;
    setup
        .send(
            sessions::REP,
            json!({ "phone_digits": "3175550100", "text": "Hi" }),
        )
        .await
        .assert_status_ok();
    setup
        .hook(&status(
            "message.finalized",
            "msg-1",
            "delivery_failed",
            json!([{ "code": "40002", "title": "Blocked as spam" }]),
        ))
        .await
        .assert_status_ok();
    let row = rows(&pool).await.remove(0);
    assert_eq!(
        (
            row.status.as_str(),
            row.delivery_status.as_deref(),
            row.error_code.as_deref()
        ),
        ("failed", Some("delivery_failed"), Some("40002"))
    );
    let thread = setup
        .get("/v1/sms/telnyx/threads/3175550100", sessions::REP)
        .await;
    assert_eq!(thread["messages"][0]["status"], "failed");

    // A status of a text we never sent: asked again while it is fresh.
    setup
        .hook(&status("message.sent", "msg-unknown", "sent", json!([])))
        .await
        .assert_status(StatusCode::CONFLICT);
}

#[test_db::test(seed)]
async fn pictures_go_as_mms_and_big_files_as_links(pool: MySqlPool) {
    let setup = setup(&pool).await;
    setup
        .storage
        .put(
            "sms-attachments/100/m/a.jpg",
            vec![1; 300_000],
            "image/jpeg",
        )
        .await
        .unwrap();
    setup
        .storage
        .put(
            "sms-attachments/100/m/plan.pdf",
            vec![2; 1_500_000],
            "application/pdf",
        )
        .await
        .unwrap();
    let res = setup
        .send(
            sessions::REP,
            json!({
                "phone_digits": "3175550100",
                "text": "Your layout",
                "attachments": [
                    { "s3_key": "sms-attachments/100/m/a.jpg", "content_type": "image/jpeg",
                      "filename": "a.jpg", "width": 4, "height": 3 },
                    { "s3_key": "sms-attachments/100/m/plan.pdf", "content_type": "application/pdf",
                      "filename": "plan.pdf" },
                ],
            }),
        )
        .await;
    res.assert_status_ok();
    let sent = setup.stub.messages();
    assert_eq!(sent.len(), 2, "the picture as MMS, the big PDF as a link");
    assert_eq!(sent[0]["text"], "Your layout");
    let media = sent[0]["media_urls"].as_array().unwrap();
    assert_eq!(media.len(), 1);
    assert!(
        media[0]
            .as_str()
            .unwrap()
            .ends_with("sms-attachments/100/m/a.jpg")
    );
    assert_eq!(sent[1].get("media_urls"), None);
    let link = sent[1]["text"].as_str().unwrap();
    assert!(link.starts_with("File 1: plan.pdf\n"), "{link}");
    assert!(link.ends_with("sms-attachments/100/m/plan.pdf"), "{link}");

    let stored = rows(&pool).await;
    assert_eq!(stored.len(), 2);
    assert_eq!(
        (stored[0].text.as_str(), stored[1].text.as_str()),
        ("Your layout", "")
    );
    let files: Vec<(i32, String)> = sqlx::query_as(
        "SELECT telnyx_sms_id, filename FROM telnyx_sms_attachments ORDER BY telnyx_sms_id, position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        files,
        [
            (stored[0].id, "a.jpg".to_string()),
            (stored[1].id, "plan.pdf".to_string())
        ]
    );
}

#[test_db::test(seed)]
async fn users_with_a_telnyx_line_text_through_telnyx(pool: MySqlPool) {
    use api::routes::sms::provider::SmsProvider;
    use api::routes::sms::send::user_provider;
    let setup = setup(&pool).await;
    let pool = &setup.state.pool;
    // Telnyx only: Rita and Mark have lines; Vic has none (no seat there).
    assert_eq!(
        user_provider(pool, 100, 100).await.unwrap(),
        Some(SmsProvider::Telnyx)
    );
    assert_eq!(
        user_provider(pool, 100, 101).await.unwrap(),
        Some(SmsProvider::Telnyx)
    );
    assert_eq!(
        user_provider(pool, 100, 104).await.unwrap(),
        Some(SmsProvider::Telnyx)
    );
    // With CloudTalk too, those without a Telnyx line keep CloudTalk.
    sqlx::query(
        "UPDATE company SET cloudtalk_access_key = 'k', cloudtalk_access_secret = 's' WHERE id = 100",
    )
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(
        user_provider(pool, 100, 100).await.unwrap(),
        Some(SmsProvider::Telnyx)
    );
    assert_eq!(
        user_provider(pool, 100, 104).await.unwrap(),
        Some(SmsProvider::Cloudtalk)
    );
    // Without the messaging profile, nobody texts through Telnyx.
    sqlx::query(
        "UPDATE telnyx_company_config SET messaging_profile_id = NULL WHERE company_id = 100",
    )
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(
        user_provider(pool, 100, 100).await.unwrap(),
        Some(SmsProvider::Cloudtalk)
    );

    // The app shell and company features say so.
    let shell = setup.get("/v1/app/shell", sessions::REP).await;
    assert_eq!(shell["has_telnyx_sms"], false);
    sqlx::query("UPDATE telnyx_company_config SET messaging_profile_id = 'profile-100' WHERE company_id = 100")
        .execute(pool)
        .await
        .unwrap();
    let shell = setup.get("/v1/app/shell", sessions::REP).await;
    assert_eq!(shell["has_telnyx_sms"], true);
    assert_eq!(shell["telnyx_line"], "+13175550151");
}

#[test_db::test(seed)]
async fn a_drip_text_goes_out_through_telnyx(pool: MySqlPool) {
    let setup = setup(&pool).await;
    sqlx::query(
        "INSERT INTO scheduled_sms (id, template_id, deal_id, list_id, customer_id, user_id, company_id, send_at)
         VALUES (9910, 8671, 8000, NULL, 1000, 100, 100, '2026-10-01 09:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE customers SET source = 'leads' WHERE id = 1000")
        .execute(&pool)
        .await
        .unwrap();
    let res = setup
        .server
        .post("/v1/jobs/sms-followups")
        .add_header("Authorization", "job-secret")
        .await;
    res.assert_status_ok();
    let sent = setup.stub.messages();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0]["from"].as_str(), sent[0]["to"].as_str()),
        (Some("+13175550151"), Some("+13175550100"))
    );
    let (status, provider_id): (String, Option<String>) = sqlx::query_as(
        "SELECT CAST(status AS CHAR), provider_message_id FROM scheduled_sms WHERE id = 9910",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), provider_id.as_deref()),
        ("sent", Some("msg-1"))
    );
    let row = rows(&pool).await.remove(0);
    assert!(row.is_automated);
    assert_eq!(row.agent.as_deref(), Some("100"));

    // A phone that texted STOP gets no drip.
    sqlx::query(
        "INSERT INTO sms_opt_outs (company_id, phone10, source, opted_out_at)
         VALUES (100, '3175550100', 'stop', UTC_TIMESTAMP())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO scheduled_sms (id, template_id, deal_id, list_id, customer_id, user_id, company_id, send_at)
         VALUES (9911, 8671, 8000, NULL, 1000, 100, 100, '2026-10-01 09:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap();
    setup
        .server
        .post("/v1/jobs/sms-followups")
        .add_header("Authorization", "job-secret")
        .await
        .assert_status_ok();
    assert_eq!(setup.stub.messages().len(), 1);
    let (status, reason): (String, Option<String>) = sqlx::query_as(
        "SELECT CAST(status AS CHAR), error_message FROM scheduled_sms WHERE id = 9911",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), reason.as_deref()),
        ("failed", Some("Customer opted out of texts (STOP)"))
    );
}

fn phones(list: &Value) -> Vec<&str> {
    list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["phone_digits"].as_str().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn a_users_inbox_is_the_threads_of_their_lines(pool: MySqlPool) {
    let setup = setup(&pool).await;
    // Alice texted Rita's line; Bob the shared line; Carol (1002) Ada's line.
    for (id, from, to, text) in [
        ("in-a", "+13175550100", "+13175550151", "Hi Rita"),
        ("in-b", "+13175550102", "+13175550152", "Hi showroom"),
        ("in-c", "+13175550103", "+13175550153", "Hi Ada"),
    ] {
        setup
            .hook(&inbound(id, from, to, text, json!([])))
            .await
            .assert_status_ok();
    }
    let rita = setup.get("/v1/sms/telnyx/threads", sessions::REP).await;
    assert_eq!(phones(&rita).len(), 2);
    assert!(phones(&rita).contains(&"3175550100"));
    assert!(phones(&rita).contains(&"3175550102"));
    assert_eq!(rita["unread_count"], 2);
    let alice = rita["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["phone_digits"] == "3175550100")
        .unwrap();
    assert_eq!(alice["customer_name"], "Alice Lead");
    assert_eq!(alice["last_message_text"], "Hi Rita");
    assert_eq!(alice["last_direction"], "inbound");
    assert_eq!(alice["unread_count"], 1);

    // Mark shares only the showroom line.
    let mark = setup.get("/v1/sms/telnyx/threads", sessions::MANAGER).await;
    assert_eq!(phones(&mark), ["3175550102"]);
    let count = setup
        .get("/v1/sms/telnyx/unread-count", sessions::MANAGER)
        .await;
    assert_eq!(count["count"], 1);
    // Carol's thread is not his.
    let hidden = setup
        .get("/v1/sms/telnyx/threads/3175550103", sessions::MANAGER)
        .await;
    assert_eq!(hidden["messages"], json!([]));

    // Opening and marking read.
    let thread = setup
        .get("/v1/sms/telnyx/threads/3175550102", sessions::MANAGER)
        .await;
    assert_eq!(thread["can_send"], true);
    assert_eq!(thread["messages"][0]["text"], "Hi showroom");
    setup
        .server
        .post("/v1/sms/telnyx/threads/3175550102/read")
        .with_session(sessions::MANAGER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let count = setup
        .get("/v1/sms/telnyx/unread-count", sessions::MANAGER)
        .await;
    assert_eq!(count["count"], 0);

    // The admin sees the company; the rep filter takes the rep's user id.
    let all = setup
        .get("/v1/sms/telnyx/threads?scope=all", sessions::ADMIN)
        .await;
    assert_eq!(all["total_count"], 3);
    let marks = setup
        .get(
            "/v1/sms/telnyx/threads?scope=all&agent_id=101",
            sessions::ADMIN,
        )
        .await;
    assert_eq!(phones(&marks), ["3175550102"]);
    let reps = setup
        .get("/v1/sms/telnyx/sales-reps", sessions::ADMIN)
        .await;
    let names: Vec<&str> = reps
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Ada Admin", "Mark Manager", "Rita Rep"]);
    // The merged admin inbox has them too.
    let merged = setup.get("/v1/sms/all/threads", sessions::ADMIN).await;
    assert_eq!(merged["total_count"], 3);

    // Desktop alerts for Rita: her two unread conversations.
    let alerts = setup
        .get("/v1/sms/telnyx/desktop-alerts", sessions::REP)
        .await;
    assert_eq!(alerts["alerts"].as_array().unwrap().len(), 2);
    assert!(
        alerts["alerts"][0]["href"]
            .as_str()
            .unwrap()
            .starts_with("/employee/telnyx/thread/")
    );
}
