//! Emails with documents to sign through `SignWell` (`POST /v1/emails` with
//! `signature_requests`), against stand-ins for `SignWell` and the
//! `webhooks` Lambda.

mod common;

use api::app;
use api::routes::emails::attachment_token;
use api::state::AppState;
use api::storage::Storage;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};
use axum_test::TestServer;
use base64::Engine;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

// Rita (100, Acme) may send for signature; Acme has a SignWell key. Alice
// (1000, alice@example.com) is the deal 8000's customer.

const FIXTURE: &str = "
UPDATE users SET is_signature_available = 1 WHERE id = 100;
UPDATE company SET signature_enabled = 1, signwell_api_key = 'sw-key', logo_url = 'https://cdn.acme.test/logo.png' WHERE id = 100;
";
const KEY: &str = "emails/100/f1/contract.pdf";

#[derive(Clone, Default)]
struct Fakes {
    documents: Arc<Mutex<Vec<(String, Value)>>>,
    cancelled: Arc<Mutex<Vec<String>>>,
    emails: Arc<Mutex<Vec<OutgoingEmail>>>,
    reject_email: Arc<Mutex<bool>>,
}

async fn create_document(
    State(fakes): State<Fakes>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let key = headers["x-api-key"].to_str().unwrap().to_string();
    let mut documents = fakes.documents.lock().unwrap();
    documents.push((key, body));
    let id = format!("doc-{}", documents.len());
    (
        StatusCode::CREATED,
        Json(
            json!({ "id": id, "status": "sent", "recipients": [{ "id": "1", "signing_url": format!("https://www.signwell.com/docs/{id}/sign") }] }),
        ),
    )
}

async fn cancel_document(State(fakes): State<Fakes>, Path(id): Path<String>) -> StatusCode {
    fakes.cancelled.lock().unwrap().push(id);
    StatusCode::NO_CONTENT
}

async fn send_email(
    State(fakes): State<Fakes>,
    Json(email): Json<OutgoingEmail>,
) -> (StatusCode, Json<Value>) {
    if *fakes.reject_email.lock().unwrap() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Email address is not verified" })),
        );
    }
    let mut emails = fakes.emails.lock().unwrap();
    emails.push(email);
    (
        StatusCode::OK,
        Json(json!({ "message_id": format!("<m{}@ses.test>", emails.len()) })),
    )
}

fn contract_pdf() -> Vec<u8> {
    let mut page = api::pdf::Page::new();
    page.text(
        "Contract for countertops",
        72.0,
        720.0,
        18.0,
        api::pdf::Font::Bold,
    );
    page.text("Customer name:", 72.0, 650.0, 12.0, api::pdf::Font::Regular);
    page.finish()
}

struct Setup {
    pool: MySqlPool,
    state: AppState,
    server: TestServer,
    fakes: Fakes,
    url: String,
}

async fn setup(pool: &MySqlPool) -> Setup {
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    let fakes = Fakes::default();
    let router = Router::new()
        .route("/documents", post(create_document))
        .route("/documents/{id}", delete(cancel_document))
        .route("/email/send", post(send_email))
        .with_state(fakes.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let storage = Storage::memory();
    storage
        .put(KEY, contract_pdf(), "application/pdf")
        .await
        .unwrap();
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_storage(storage)
        .with_signwell_base(&base)
        .with_webhooks(WebhooksClient::new(Some(&base), Some("webhooks-key")));
    Setup {
        pool: pool.clone(),
        server: TestServer::new(app(state.clone())),
        state,
        fakes,
        url: format!("https://files.test/bucket/{KEY}"),
    }
}

fn signature_png() -> String {
    let mut image = image::RgbaImage::new(60, 20);
    for x in 5..55 {
        image.put_pixel(x, 10, image::Rgba([20, 40, 200, 255]));
    }
    let mut out = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(out)
    )
}

impl Setup {
    fn email(&self, extra: Value) -> Value {
        let mut body = json!({
            "to": ["alice@example.com"],
            "subject": "Your contract",
            "body": "<p>Please sign your contract.</p>",
            "deal_id": 8000,
            "attachments": [
                { "url": self.url, "filename": "contract.pdf", "content_type": "application/pdf",
                  "token": attachment_token::sign(SECRET, &self.url, 100) }
            ],
            "signature_requests": [{
                "filename": "contract.pdf", "url": self.url, "signer_name": "Alice Lead",
                "signer_email": "alice@example.com", "reminders": true,
                "fields": [
                    { "type": "signature", "page": 1, "x": 72, "y": 600, "width": 180, "height": 40, "required": true },
                    { "type": "text", "page": 1, "x": 170, "y": 130, "width": 200, "height": 18, "required": true,
                      "value": "Alice Lead", "prefill": "customer_name" },
                    { "type": "signature", "page": 1, "x": 320, "y": 600, "width": 180, "height": 40, "required": true,
                      "value": signature_png() }
                ]
            }],
        });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        body
    }

    async fn send(&self, session: &str, body: Value) -> axum_test::TestResponse {
        self.server
            .post("/v1/emails")
            .with_session(session)
            .json(&body)
            .await
    }
}

#[test_db::test(seed)]
async fn a_contract_goes_to_signwell_and_its_link_in_the_email(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let res = setup.send(sessions::REP, setup.email(json!({}))).await;
    res.assert_status_ok();

    let documents = setup.fakes.documents.lock().unwrap().clone();
    assert_eq!(documents.len(), 1);
    let (key, document) = &documents[0];
    assert_eq!(key, "sw-key");
    assert_eq!(document["name"], "contract");
    assert_eq!(document["recipients"][0]["email"], "alice@example.com");
    assert_eq!(document["recipients"][0]["send_email"], false);
    assert_eq!(document["reminders"], true);
    assert_eq!(
        document["metadata"],
        json!({ "company_id": "100", "customer_id": "1000", "deal_id": "8000" })
    );
    // Only the customer's signature goes to SignWell, in 96 DPI pixels; the
    // filled name and Rita's signature are printed on the PDF.
    assert_eq!(
        document["fields"],
        json!([[{ "type": "signature", "required": true, "page": 1, "x": 96, "y": 800, "width": 240, "height": 53, "recipient_id": "1" }]])
    );
    let sent_pdf = base64::engine::general_purpose::STANDARD
        .decode(document["files"][0]["file_base64"].as_str().unwrap())
        .unwrap();
    assert!(sent_pdf.starts_with(b"%PDF"));
    assert_ne!(sent_pdf, contract_pdf());
    assert!(document["files"][0].get("file_url").is_none());

    let emails = setup.fakes.emails.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    let email = &emails[0];
    assert!(
        email.body.contains("Please Review This Document"),
        "{}",
        email.body
    );
    assert!(
        email
            .body
            .contains("href=\"https://www.signwell.com/docs/doc-1/sign\"")
    );
    assert!(email.body.contains("Please sign your contract."));
    assert!(email.body.contains("https://cdn.acme.test/logo.png"));
    // The unsigned PDF is not attached: the link is the document.
    assert!(email.attachments.is_empty());

    let row: (String, String, String, Option<i32>, Option<i32>, i32, String) = sqlx::query_as(
        "SELECT CAST(sr.status AS CHAR), sr.provider, sr.provider_document_id, sr.email_attachment_id, sr.customer_id,
                sr.deal_id, sr.signer_email
           FROM signature_requests sr JOIN emails e ON e.id = sr.email_id
          WHERE e.subject = 'Your contract'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(&row.0, "sent");
    assert_eq!((row.1.as_str(), row.2.as_str()), ("signwell", "doc-1"));
    assert!(row.3.is_some());
    assert_eq!(
        (row.4, row.5, row.6.as_str()),
        (Some(1000), 8000, "alice@example.com")
    );
    let attachment: (String, String) = sqlx::query_as(
        "SELECT ea.filename, ea.url FROM email_attachments ea JOIN signature_requests sr ON sr.email_attachment_id = ea.id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attachment, ("contract.pdf".to_string(), setup.url.clone()));
}

#[test_db::test(seed)]
async fn an_empty_signature_email_gets_a_subject_and_a_message(pool: MySqlPool) {
    let setup = setup(&pool).await;
    setup
        .send(
            sessions::REP,
            setup.email(json!({ "subject": "", "body": "<p><br></p>" })),
        )
        .await
        .assert_status_ok();
    let email = setup.fakes.emails.lock().unwrap()[0].clone();
    assert_eq!(email.subject, "Signature requested on \"contract\"");
    assert!(email.body.contains("Please review and sign the document."));
}

#[test_db::test(seed)]
async fn signatures_need_access_and_one_message(pool: MySqlPool) {
    let setup = setup(&pool).await;
    sqlx::query("UPDATE users SET is_signature_available = 0 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let res = setup.send(sessions::REP, setup.email(json!({}))).await;
    res.assert_status(StatusCode::FORBIDDEN);
    assert!(
        res.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .starts_with("You do not have access to send documents for signature.")
    );

    sqlx::query("UPDATE users SET is_signature_available = 1 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    // The old company switch no longer matters: access is per user, so the
    // send gets past the access check to the recipient check.
    sqlx::query("UPDATE company SET signature_enabled = 0 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let res = setup
        .send(
            sessions::REP,
            setup.email(json!({ "to": ["alice@example.com", "bob@example.com"] })),
        )
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        res.json::<Value>()["error"],
        "Send signature emails as one message so the signature request is not duplicated."
    );
    let mut missing = setup.email(json!({}));
    missing["signature_requests"][0]["filename"] = "other.pdf".into();
    missing["signature_requests"][0]["url"] = Value::Null;
    let res = setup.send(sessions::REP, missing).await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        res.json::<Value>()["error"],
        "Could not find the signature document other.pdf among the email attachments."
    );
    // Rita signing every signature leaves nothing for Alice to sign.
    let mut signed = setup.email(json!({}));
    signed["signature_requests"][0]["fields"] = json!([
        { "type": "signature", "page": 1, "x": 320, "y": 600, "width": 180, "height": 40, "required": true, "value": signature_png() }
    ]);
    let res = setup.send(sessions::REP, signed).await;
    res.assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(
        res.json::<Value>()["error"],
        "contract.pdf needs at least one signature for the customer. You signed them all."
    );
    assert!(setup.fakes.emails.lock().unwrap().is_empty());
    assert!(setup.fakes.documents.lock().unwrap().is_empty());
}

#[test_db::test(seed)]
async fn a_refused_email_deletes_the_signwell_document(pool: MySqlPool) {
    let setup = setup(&pool).await;
    *setup.fakes.reject_email.lock().unwrap() = true;
    let res = setup.send(sessions::REP, setup.email(json!({}))).await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(setup.fakes.documents.lock().unwrap().len(), 1);
    assert_eq!(setup.fakes.cancelled.lock().unwrap().clone(), ["doc-1"]);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM signature_requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[test_db::test(seed)]
async fn a_scheduled_contract_is_sent_by_the_job(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let later = chrono::Utc::now() + chrono::Duration::hours(2);
    let res = setup
        .send(sessions::REP, setup.email(json!({ "send_at": later })))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["delayed"], true);
    let payload: String =
        sqlx::query_scalar("SELECT CAST(payload_json AS CHAR) FROM delayed_emails")
            .fetch_one(&pool)
            .await
            .unwrap();
    let payload: Value = serde_json::from_str(&payload).unwrap();
    let requests: Value =
        serde_json::from_str(payload["signatureRequests"].as_str().unwrap()).unwrap();
    // The CRM's shape, which its cron reads too.
    assert_eq!(requests[0]["signerName"], "Alice Lead");
    assert_eq!(requests[0]["signerEmail"], "alice@example.com");
    assert!(setup.fakes.documents.lock().unwrap().is_empty());

    sqlx::query("UPDATE delayed_emails SET send_at = UTC_TIMESTAMP() - INTERVAL 1 MINUTE")
        .execute(&pool)
        .await
        .unwrap();
    let run = api::routes::delayed::process_due_in(&setup.state, None)
        .await
        .unwrap();
    assert_eq!(run.emails, 1);
    assert_eq!(setup.fakes.documents.lock().unwrap().len(), 1);
    assert_eq!(setup.fakes.emails.lock().unwrap().len(), 1);
    let status: String = sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM delayed_emails")
        .fetch_one(&setup.pool)
        .await
        .unwrap();
    assert_eq!(status, "sent");
    let requests: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM signature_requests WHERE provider = 'signwell'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(requests, 1);
}
