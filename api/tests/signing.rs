//! The signer's side: an emailed native invitation is traded for a session,
//! reviewed, signed (the signed copy stored privately and mailed to the
//! signer) or declined; `SignWell` documents get their embedded link.

mod common;

use api::app;
use api::native_signing::NativeSigning;
use api::routes::emails::attachment_token;
use api::state::AppState;
use api::storage::Storage;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex, Once};

const FIXTURE: &str = "
UPDATE users SET is_signature_available = 1 WHERE id = 100;
UPDATE company SET signature_enabled = 1 WHERE id = 100;
";
const KEY: &str = "emails/100/f1/contract.pdf";
const INVITATION_SECRET: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
const SESSION_SECRET: &str = "session-secret-session-secret-0123456789";
const ORIGIN: &str = "https://sign.acme.test";
const SIGNWELL_DOC: &str = "5f0e7a8b-1c2d-4e3f-9a0b-1c2d3e4f5a6b";

static SETTINGS: Once = Once::new();

fn verified_settings() {
    SETTINGS.call_once(|| {
        let settings = json!({ "100": {
            "verifiedAt": "2026-10-01T00:00:00Z",
            "evidenceReference": "ticket-1",
            "settings": {
                "parityVerified": true, "timezone": "America/Indiana/Indianapolis", "signatureMethods": ["type", "draw"],
                "allowDecline": true, "allowReassign": false, "authentication": "invitation", "reminders": false,
                "expiresInDays": 30, "notifications": { "viewed": false, "completed": false, "declined": false },
                "auditPage": true
            }
        }});
        // SAFETY: set once, before any test of this binary reads it.
        unsafe { std::env::set_var("SIGNATURE_VERIFIED_COMPANY_SETTINGS", settings.to_string()) };
    });
}

#[derive(Clone, Default)]
struct Fakes {
    emails: Arc<Mutex<Vec<OutgoingEmail>>>,
}

async fn send_email(State(fakes): State<Fakes>, Json(email): Json<OutgoingEmail>) -> Json<Value> {
    let mut emails = fakes.emails.lock().unwrap();
    emails.push(email);
    Json(json!({ "message_id": format!("<n{}@ses.test>", emails.len()) }))
}

async fn signwell_document(Path(id): Path<String>, headers: HeaderMap) -> Json<Value> {
    assert_eq!(headers["x-api-key"], "sw-key");
    assert_eq!(id, SIGNWELL_DOC);
    Json(
        json!({ "recipients": [{ "embedded_signing_url": "https://www.signwell.com/docs/abc/sign" }] }),
    )
}

struct Setup {
    server: TestServer,
    fakes: Fakes,
    storage: Storage,
    pool: MySqlPool,
}

async fn setup(pool: &MySqlPool) -> Setup {
    verified_settings();
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    let fakes = Fakes::default();
    let router = Router::new()
        .route("/email/send", post(send_email))
        .route("/documents/{id}", get(signwell_document))
        .with_state(fakes.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut page = api::pdf::Page::new();
    page.text("Contract", 72.0, 720.0, 18.0, api::pdf::Font::Bold);
    let storage = Storage::memory();
    storage
        .put(KEY, page.finish(), "application/pdf")
        .await
        .unwrap();
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_storage(storage.clone())
        .with_signature_bucket("sig-bucket")
        .with_signature_app_origin(ORIGIN)
        .with_signing_session_secret(SESSION_SECRET)
        .with_signwell_base(&base)
        .with_native_signing(Some(NativeSigning::new(INVITATION_SECRET, ORIGIN).unwrap()))
        .with_webhooks(WebhooksClient::new(Some(&base), Some("webhooks-key")));
    let mut server = TestServer::new(app(state));
    server.save_cookies();
    Setup {
        server,
        fakes,
        storage,
        pool: pool.clone(),
    }
}

impl Setup {
    /// Emails a native invitation; its document id and token.
    async fn invite(&self) -> (String, String) {
        let url = format!("https://files.test/bucket/{KEY}");
        self.server
            .post("/v1/emails")
            .with_session(sessions::REP)
            .json(&json!({
                "to": ["alice@example.com"],
                "subject": "Your contract",
                "body": "Please sign.",
                "deal_id": 8000,
                "signature_send_key": "6f1d2c3b-1a2b-4c5d-8e9f-0a1b2c3d4e5f",
                "attachments": [{ "url": url, "filename": "contract.pdf", "content_type": "application/pdf",
                                  "token": attachment_token::sign(SECRET, &url, 100) }],
                "signature_requests": [{
                    "filename": "contract.pdf", "url": url, "signer_name": "Alice Lead", "signer_email": "alice@example.com",
                    "reminders": false,
                    "fields": [
                        { "type": "signature", "page": 1, "x": 72, "y": 600, "width": 180, "height": 40, "required": true },
                        { "type": "text", "page": 1, "x": 72, "y": 500, "width": 200, "height": 18, "required": true, "value": "Alice Lead" }
                    ]
                }],
            }))
            .await
            .assert_status_ok();
        let body = self.fakes.emails.lock().unwrap()[0].body.clone();
        let start = body
            .find(&format!("{ORIGIN}/sign/"))
            .expect("an invitation link")
            + ORIGIN.len()
            + 6;
        let link = &body[start..];
        let (id, rest) = link.split_once("?token=").unwrap();
        let token: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        self.fakes.emails.lock().unwrap().clear();
        (id.to_string(), token)
    }

    async fn open(&self, id: &str, token: &str) -> Value {
        self.server
            .post(&format!("/v1/public/signing/{id}/session"))
            .json(&json!({ "token": token }))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        let res = self.server.get(&format!("/v1/public/signing/{id}")).await;
        res.assert_status_ok();
        let page = res.json::<Value>();
        assert_eq!(page["provider"], "native");
        page["view"].clone()
    }

    async fn act(&self, id: &str, body: Value) -> axum_test::TestResponse {
        self.server
            .post(&format!("/v1/public/signing/{id}/actions"))
            .add_header("origin", ORIGIN)
            .json(&body)
            .await
    }

    async fn status(&self) -> String {
        sqlx::query_scalar(
            "SELECT CAST(status AS CHAR) FROM signature_requests WHERE provider = 'native'",
        )
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
}

fn submission(view: &Value, key: &str) -> Value {
    let signature = view["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["type"] == "signature")
        .unwrap()["id"]
        .clone();
    json!({
        "documentVersion": view["document_version"],
        "consent": true,
        "consentVersion": "native-v1",
        "idempotencyKey": key,
        "answers": { signature.as_str().unwrap(): format!("data:image/png;base64,{}", STANDARD.encode(common::png(120, 40))) },
    })
}

#[test_db::test(seed)]
async fn a_signer_reviews_and_signs_a_native_document(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let (id, token) = setup.invite().await;
    let view = setup.open(&id, &token).await;
    assert_eq!(view["status"], "viewed");
    assert_eq!(view["signer_name"], "Alice Lead");
    assert_eq!(view["reviewed"], false);
    assert_eq!(view["signature_methods"], json!(["type", "draw"]));
    assert_eq!(setup.status().await, "viewed");

    // The document to read is served to the session.
    let prepared = setup
        .server
        .get(&format!("/v1/public/signing/{id}/download?kind=prepared"))
        .await;
    prepared.assert_status_ok();
    assert_eq!(
        prepared.header("cache-control"),
        "private, no-store, max-age=0"
    );
    assert!(prepared.as_bytes().starts_with(b"%PDF"));

    // Finishing needs a review first, the right origin and the CSRF token.
    let finish = json!({ "intent": "finish", "csrf": view["csrf"], "submission": submission(&view, "finish-key-0001-abcd") });
    let res = setup.act(&id, finish.clone()).await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(res.json::<Value>()["code"], "review_required");
    let res = setup
        .act(&id, json!({ "intent": "start", "csrf": "wrong" }))
        .await;
    res.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(res.json::<Value>()["code"], "csrf");
    setup
        .server
        .post(&format!("/v1/public/signing/{id}/actions"))
        .add_header("origin", "https://evil.test")
        .json(&json!({ "intent": "start", "csrf": view["csrf"] }))
        .await
        .assert_status(StatusCode::FORBIDDEN);

    let res = setup
        .act(&id, json!({ "intent": "start", "csrf": view["csrf"] }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["view"]["reviewed"], true);

    // A missing answer is refused before anything is recorded.
    let mut empty = submission(&view, "finish-key-0001-abcd");
    empty["answers"] = json!({});
    let res = setup
        .act(
            &id,
            json!({ "intent": "finish", "csrf": view["csrf"], "submission": empty }),
        )
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["code"], "invalid_submission");

    let res = setup.act(&id, finish.clone()).await;
    res.assert_status_ok();
    let signed = res.json::<Value>()["view"].clone();
    assert_eq!(signed["status"], "signed");
    assert_eq!(signed["finish_pending"], false);
    assert_eq!(setup.status().await, "signed");

    // The signed copy is stored privately with its evidence, and mailed.
    let (key, sha, state): (String, String, String) = sqlx::query_as(
        "SELECT nd.completed_object_key, nd.completed_sha256, CAST(f.state AS CHAR)
           FROM native_signature_documents nd
           JOIN native_signature_finalizations f ON f.request_id = nd.request_id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "completed");
    assert_eq!(key, format!("100/signatures/{id}/completed-{sha}.pdf"));
    let stored = setup.storage.get(&key).await.unwrap().unwrap();
    assert_eq!(api::native_signing::sha256_hex(&stored.bytes), sha);
    let events: Vec<String> =
        sqlx::query_scalar("SELECT event_type FROM native_signature_audit ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        events.ends_with(&["consent".into(), "completed".into()]),
        "{events:?}"
    );
    let emails = setup.fakes.emails.lock().unwrap().clone();
    let copy = emails
        .iter()
        .find(|e| e.subject == "You signed: \"contract\"")
        .expect("the signer's copy");
    assert_eq!(copy.to, vec!["alice@example.com".to_string()]);
    assert_eq!(copy.attachments.len(), 1);

    let completed = setup
        .server
        .get(signed["completed_url"].as_str().unwrap())
        .await;
    completed.assert_status_ok();
    assert_eq!(completed.as_bytes().to_vec(), stored.bytes);

    // Finishing again with the same answers is a no-op; other answers conflict.
    setup.act(&id, finish).await.assert_status_ok();
    let res = setup
        .act(&id, json!({ "intent": "finish", "csrf": view["csrf"], "submission": submission(&view, "another-key-0002-abcd") }))
        .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(res.json::<Value>()["code"], "idempotency_conflict");
}

#[test_db::test(seed)]
async fn a_signer_can_decline(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let (id, token) = setup.invite().await;
    let view = setup.open(&id, &token).await;
    assert_eq!(view["allow_decline"], true);
    let res = setup
        .act(
            &id,
            json!({ "intent": "decline", "csrf": view["csrf"], "reason": "Wrong price" }),
        )
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["view"]["status"], "declined");
    assert_eq!(setup.status().await, "declined");
    let reason: String = sqlx::query_scalar(
        "SELECT JSON_UNQUOTE(JSON_EXTRACT(evidence_json, '$.reason')) FROM native_signature_audit WHERE event_type = 'declined'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reason, "Wrong price");
    // The document can no longer be read for signing.
    setup
        .server
        .get(&format!("/v1/public/signing/{id}/download?kind=prepared"))
        .await
        .assert_status(StatusCode::CONFLICT);
}

#[test_db::test(seed)]
async fn bad_links_and_missing_sessions_are_refused(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let (id, token) = setup.invite().await;
    let wrong = "A".repeat(43);
    let res = setup
        .server
        .post(&format!("/v1/public/signing/{id}/session"))
        .json(&json!({ "token": wrong }))
        .await;
    res.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(
        res.json::<Value>()["error"],
        "This signing link is not valid."
    );
    // No session yet: the native view needs one.
    setup
        .server
        .get(&format!("/v1/public/signing/{id}"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    setup
        .server
        .get("/v1/public/signing/not-a-uuid")
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // An expired link cannot be opened.
    sqlx::query("UPDATE native_signature_documents SET expires_at = NOW() - INTERVAL 1 DAY")
        .execute(&pool)
        .await
        .unwrap();
    let res = setup
        .server
        .post(&format!("/v1/public/signing/{id}/session"))
        .json(&json!({ "token": token }))
        .await;
    res.assert_status(StatusCode::GONE);
    assert_eq!(res.json::<Value>()["code"], "expired");
}

#[test_db::test(seed)]
async fn signwell_documents_open_their_embedded_page(pool: MySqlPool) {
    let setup = setup(&pool).await;
    sqlx::raw_sql("UPDATE company SET signwell_api_key = 'sw-key' WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO signature_requests (company_id, customer_id, created_by_user_id, filename, file_url,
             signer_name, signer_email, status, provider, provider_document_id)
         VALUES (100, 1000, 100, 'Quote.pdf', 'https://files.test/q.pdf', 'John Clay', 'john@example.com', 'sent', 'signwell', ?)",
    )
    .bind(SIGNWELL_DOC)
    .execute(&pool)
    .await
    .unwrap();
    let res = setup
        .server
        .get(&format!("/v1/public/signing/{SIGNWELL_DOC}"))
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({ "provider": "signwell", "signing_url": "https://www.signwell.com/docs/abc/sign" })
    );
}
