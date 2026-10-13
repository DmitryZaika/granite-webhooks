//! Emails with documents to sign natively (`NATIVE_SIGNATURES_ENABLED`):
//! prepared and stored privately, the invitation claimed, sent once and
//! recorded with the email.

mod common;

use api::app;
use api::native_signing::NativeSigning;
use api::routes::emails::attachment_token;
use api::state::AppState;
use api::storage::Storage;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex, Once};

const FIXTURE: &str = "
UPDATE users SET is_signature_available = 1 WHERE id = 100;
UPDATE company SET signature_enabled = 1 WHERE id = 100;
";
const KEY: &str = "emails/100/f1/contract.pdf";
const SEND_KEY: &str = "6f1d2c3b-1a2b-4c5d-8e9f-0a1b2c3d4e5f";
const INVITATION_SECRET: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

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
    reject: Arc<Mutex<bool>>,
}

async fn send_email(
    State(fakes): State<Fakes>,
    Json(email): Json<OutgoingEmail>,
) -> (StatusCode, Json<Value>) {
    if *fakes.reject.lock().unwrap() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Address blocked" })),
        );
    }
    let mut emails = fakes.emails.lock().unwrap();
    emails.push(email);
    (
        StatusCode::OK,
        Json(json!({ "message_id": format!("<n{}@ses.test>", emails.len()) })),
    )
}

struct Setup {
    server: TestServer,
    fakes: Fakes,
    storage: Storage,
    url: String,
}

async fn setup(pool: &MySqlPool) -> Setup {
    verified_settings();
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    let fakes = Fakes::default();
    let router = Router::new()
        .route("/email/send", post(send_email))
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
        .with_native_signing(Some(
            NativeSigning::new(INVITATION_SECRET, "https://sign.acme.test").unwrap(),
        ))
        .with_webhooks(WebhooksClient::new(Some(&base), Some("webhooks-key")));
    Setup {
        server: TestServer::new(app(state)),
        fakes,
        storage,
        url: format!("https://files.test/bucket/{KEY}"),
    }
}

impl Setup {
    fn email(&self) -> Value {
        json!({
            "to": ["alice@example.com"],
            "subject": "Your contract",
            "body": "Please sign.",
            "deal_id": 8000,
            "signature_send_key": SEND_KEY,
            "attachments": [{ "url": self.url, "filename": "contract.pdf", "content_type": "application/pdf",
                              "token": attachment_token::sign(SECRET, &self.url, 100) }],
            "signature_requests": [{
                "filename": "contract.pdf", "url": self.url, "signer_name": "Alice Lead", "signer_email": "alice@example.com",
                "reminders": true,
                "fields": [
                    { "type": "signature", "page": 1, "x": 72, "y": 600, "width": 180, "height": 40, "required": true },
                    { "type": "text", "page": 1, "x": 72, "y": 500, "width": 200, "height": 18, "required": true, "value": "Alice Lead" }
                ]
            }],
        })
    }

    async fn send(&self) -> axum_test::TestResponse {
        self.server
            .post("/v1/emails")
            .with_session(sessions::REP)
            .json(&self.email())
            .await
    }
}

#[test_db::test(seed)]
async fn a_native_invitation_is_prepared_sent_and_recorded_once(pool: MySqlPool) {
    let setup = setup(&pool).await;
    setup.send().await.assert_status_ok();
    let emails = setup.fakes.emails.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    let (document_id, status, email_id, attachment_id): (String, String, Option<i32>, Option<i32>) = sqlx::query_as(
        "SELECT provider_document_id, CAST(status AS CHAR), email_id, email_attachment_id FROM signature_requests WHERE provider = 'native'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "sent");
    assert!(email_id.is_some() && attachment_id.is_some());
    assert!(
        emails[0]
            .body
            .contains(&format!("https://sign.acme.test/sign/{document_id}?token=")),
        "{}",
        emails[0].body
    );
    assert!(emails[0].attachments.is_empty());

    let (state, key, sha, settings): (String, String, String, String) = sqlx::query_as(
        "SELECT CAST(d.state AS CHAR), nd.prepared_object_key, nd.prepared_sha256, CAST(nd.settings_json AS CHAR)
           FROM native_signature_deliveries d
           JOIN native_signature_delivery_items di ON di.delivery_id = d.id
           JOIN native_signature_documents nd ON nd.request_id = di.request_id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "recorded");
    assert_eq!(
        key,
        format!("100/signatures/{document_id}/prepared-{sha}.pdf")
    );
    let stored = setup.storage.get(&key).await.unwrap().unwrap();
    assert_eq!(api::native_signing::sha256_hex(&stored.bytes), sha);
    // The send's reminder choice is frozen with the company's settings.
    assert_eq!(
        serde_json::from_str::<Value>(&settings).unwrap()["reminders"],
        true
    );

    // A retried send (same key, same contents) neither mails nor records again.
    setup.send().await.assert_status_ok();
    assert_eq!(setup.fakes.emails.lock().unwrap().len(), 1);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM emails WHERE subject = 'Your contract'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    // The same key with other contents is refused.
    let mut changed = setup.email();
    changed["body"] = "Changed".into();
    let res = setup
        .server
        .post("/v1/emails")
        .with_session(sessions::REP)
        .json(&changed)
        .await;
    res.assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(
        res.json::<Value>()["error"],
        "Signature send contents changed; start a new send"
    );
}

#[test_db::test(seed)]
async fn a_refused_invitation_revokes_its_links(pool: MySqlPool) {
    let setup = setup(&pool).await;
    *setup.fakes.reject.lock().unwrap() = true;
    let res = setup.send().await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        res.json::<Value>()["error"],
        "Email was rejected. Unsent signing links were revoked."
    );
    let (status, revoked, state): (String, bool, String) = sqlx::query_as(
        "SELECT CAST(sr.status AS CHAR), nd.revoked_at IS NOT NULL, CAST(d.state AS CHAR)
           FROM signature_requests sr
           JOIN native_signature_documents nd ON nd.request_id = sr.id
           JOIN native_signature_delivery_items di ON di.request_id = sr.id
           JOIN native_signature_deliveries d ON d.id = di.delivery_id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), revoked, state.as_str()),
        ("expired", true, "rejected")
    );
}

#[test_db::test(seed)]
async fn a_native_send_needs_its_key(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let mut body = setup.email();
    body.as_object_mut().unwrap().remove("signature_send_key");
    let res = setup
        .server
        .post("/v1/emails")
        .with_session(sessions::REP)
        .json(&body)
        .await;
    res.assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(
        res.json::<Value>()["error"],
        "A stable logical send identity is required"
    );
}
