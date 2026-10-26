use crate::common;

use api::app;
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
use std::sync::{Arc, Mutex};

// Seed: Dina (107, company 100, no company domain), deal 8600 of Nina
// (customer 8695: nina@example.com, nina.work@example.com), Alice (customer
// 1000: alice@example.com), Olga's thread T1 (company 101, attachment 8810).

const DINA: &str = sessions::DEALER;
const OLGA: &str = sessions::OTHER_COMPANY_REP;
const T1: &str = "aaaaaaaa-1111-4000-8000-000000000001";

/// What the fake `webhooks` Lambda answers.
#[derive(Clone)]
enum Reply {
    Sent,
    Rejected(&'static str),
    Down,
}

#[derive(Clone)]
struct Fake {
    sent: Arc<Mutex<Vec<OutgoingEmail>>>,
    reply: Reply,
}

async fn fake_send(
    State(fake): State<Fake>,
    headers: axum::http::HeaderMap,
    Json(email): Json<OutgoingEmail>,
) -> (StatusCode, Json<Value>) {
    assert_eq!(headers["authorization"], "webhooks-key");
    match fake.reply {
        Reply::Sent => {
            let mut sent = fake.sent.lock().unwrap();
            sent.push(email);
            let id = format!("010f-test-{}@us-east-2.amazonses.com", sent.len());
            (StatusCode::OK, Json(json!({ "message_id": id })))
        }
        Reply::Rejected(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
        Reply::Down => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Email failed to send" })),
        ),
    }
}

struct Setup {
    server: TestServer,
    sent: Arc<Mutex<Vec<OutgoingEmail>>>,
}

async fn setup(pool: &MySqlPool, reply: Reply) -> Setup {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let fake = Fake {
        sent: Arc::clone(&sent),
        reply,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/email/send", post(fake_send))
        .with_state(fake);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_storage(Storage::memory())
        .with_webhooks(WebhooksClient::new(
            Some(&format!("http://{address}")),
            Some("webhooks-key"),
        ));
    Setup {
        server: TestServer::new(app(state)),
        sent,
    }
}

impl Setup {
    async fn send(&self, session: &str, body: Value) -> axum_test::TestResponse {
        self.server
            .post("/v1/emails")
            .with_session(session)
            .json(&body)
            .await
    }

    fn sent(&self) -> Vec<OutgoingEmail> {
        self.sent.lock().unwrap().clone()
    }
}

#[derive(sqlx::FromRow, Debug)]
struct Recorded {
    id: i32,
    sender_user_id: Option<i32>,
    subject: Option<String>,
    body: String,
    message_id: Option<String>,
    sender_email: Option<String>,
    receiver_email: Option<String>,
    thread_id: Option<String>,
    deal_id: Option<u64>,
    company_id: Option<i32>,
}

async fn recorded(pool: &MySqlPool, user_id: i32) -> Vec<Recorded> {
    sqlx::query_as(
        "SELECT id, sender_user_id, subject, body, message_id, sender_email, receiver_email,
                thread_id, deal_id, company_id
         FROM emails WHERE sender_user_id = ? AND id > 9000 ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn participants(
    pool: &MySqlPool,
    email_id: i32,
) -> Vec<(String, String, Option<i32>, Option<i32>)> {
    sqlx::query_as(
        "SELECT type, email, user_id, customer_id FROM email_participants
         WHERE email_id = ? ORDER BY FIELD(type, 'from', 'to', 'cc', 'bcc'), position",
    )
    .bind(email_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn reset_ids(pool: &MySqlPool) {
    sqlx::query("ALTER TABLE emails AUTO_INCREMENT = 9001")
        .execute(pool)
        .await
        .unwrap();
}

#[test_db::test(seed)]
async fn sends_one_message_and_records_it_on_the_deal(pool: MySqlPool) {
    reset_ids(&pool).await;
    let setup = setup(&pool, Reply::Sent).await;
    let res = setup
        .send(
            DINA,
            json!({
                "to": ["Nina@Example.com"],
                "cc": ["rep@acme.test", "nina@example.com"],
                "subject": " Your quote ",
                "body": "<p>Hi {{customer.first_name}}</p>",
                "deal_id": 8600,
            }),
        )
        .await;
    res.assert_status_ok();
    let result: Value = res.json();
    assert_eq!(result["sent"], 1);
    assert_eq!(result["delayed"], false);
    let thread_id = result["thread_ids"][0].as_str().unwrap().to_string();
    assert_eq!(thread_id.len(), 36);

    let sent = setup.sent();
    assert_eq!(sent.len(), 1);
    let email = &sent[0];
    assert_eq!(email.from_address, "sales@granite-manager.com");
    assert_eq!(email.to, ["nina@example.com"]);
    assert_eq!(email.cc, ["rep@acme.test"]);
    assert_eq!(email.subject, "Your quote");
    assert_eq!(email.body, "<p>Hi Nina</p>");
    assert!(email.in_reply_to.is_none());
    assert!(email.references.is_empty());

    let rows = recorded(&pool, 107).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.sender_user_id, Some(107));
    assert_eq!(row.subject.as_deref(), Some("Your quote"));
    assert_eq!(row.body, "<p>Hi Nina</p>");
    assert_eq!(
        row.message_id.as_deref(),
        Some("010f-test-1@us-east-2.amazonses.com")
    );
    assert_eq!(
        row.sender_email.as_deref(),
        Some("sales@granite-manager.com")
    );
    assert_eq!(row.receiver_email.as_deref(), Some("nina@example.com"));
    assert_eq!(row.thread_id.as_deref(), Some(thread_id.as_str()));
    assert_eq!(row.deal_id, Some(8600));
    assert_eq!(row.company_id, Some(100));
    assert_eq!(
        participants(&pool, row.id).await,
        vec![
            (
                "from".into(),
                "sales@granite-manager.com".into(),
                None,
                None
            ),
            ("to".into(), "nina@example.com".into(), None, Some(8695)),
            ("cc".into(), "rep@acme.test".into(), Some(100), None),
        ]
    );
}

#[test_db::test(seed)]
async fn several_recipients_get_separate_conversations(pool: MySqlPool) {
    reset_ids(&pool).await;
    let setup = setup(&pool, Reply::Sent).await;
    let res = setup
        .send(
            DINA,
            json!({ "to": ["a@one.test", "b@two.test"], "subject": "Hi", "body": "Hello" }),
        )
        .await;
    res.assert_status_ok();
    let result: Value = res.json();
    assert_eq!(result["sent"], 2);
    let threads = result["thread_ids"].as_array().unwrap();
    assert_ne!(threads[0], threads[1]);
    let sent = setup.sent();
    assert_eq!(sent[0].to, ["a@one.test"]);
    assert_eq!(sent[1].to, ["b@two.test"]);
    assert_eq!(recorded(&pool, 107).await.len(), 2);

    let together = setup
        .send(
            DINA,
            json!({ "to": ["a@one.test", "b@two.test"], "body": "Hi", "send_together": true }),
        )
        .await;
    assert_eq!(together.json::<Value>()["sent"], 1);
    assert_eq!(setup.sent()[2].to, ["a@one.test", "b@two.test"]);
}

#[test_db::test(seed)]
async fn reply_threads_after_the_conversation(pool: MySqlPool) {
    reset_ids(&pool).await;
    let setup = setup(&pool, Reply::Sent).await;
    let res = setup
        .send(
            OLGA,
            json!({ "to": ["buyer@other-customer.test"], "body": "Here you go", "thread_id": T1 }),
        )
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["thread_ids"], json!([T1]));
    let email = &setup.sent()[0];
    assert_eq!(
        email.in_reply_to.as_deref(),
        Some("<m8802@other-customer.test>")
    );
    assert_eq!(
        email.references,
        [
            "<m8800@other-customer.test>",
            "<m8801@other.test>",
            "<m8802@other-customer.test>"
        ]
    );
    let rows = recorded(&pool, 106).await;
    assert_eq!(rows[0].thread_id.as_deref(), Some(T1));
}

#[test_db::test(seed)]
async fn deal_customer_gets_new_recipient_addresses_only(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Sent).await;
    setup
        .send(
            DINA,
            json!({
                "to": ["nina.new@example.com", "alice@example.com"],
                "body": "Hi",
                "deal_id": 8600,
                "send_together": true,
            }),
        )
        .await
        .assert_status_ok();
    let emails: Vec<String> = sqlx::query_scalar(
        "SELECT email FROM customers_emails WHERE customer_id = 8695 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    // Alice's address belongs to another customer: it is not copied.
    assert_eq!(
        emails,
        [
            "nina@example.com",
            "nina.work@example.com",
            "nina.new@example.com"
        ]
    );
}

#[test_db::test(seed)]
async fn attachments_need_a_token_or_a_readable_thread(pool: MySqlPool) {
    reset_ids(&pool).await;
    let setup = setup(&pool, Reply::Sent).await;
    let url = "https://files.test/bucket/emails/quote.pdf";
    let attachment = |token: Option<String>| json!({ "url": url, "filename": "quote.pdf", "content_type": "application/pdf", "token": token });
    let forged = setup
        .send(
            DINA,
            json!({ "to": ["a@one.test"], "attachments": [attachment(Some("00".repeat(32)))] }),
        )
        .await;
    forged.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        forged.json::<Value>()["error"],
        "Attachment is not available: quote.pdf"
    );

    // Olga's company's attachment is not Dina's to send.
    let foreign = json!({
        "url": "https://example.test/sketch.png", "filename": "sketch.png", "content_type": "image/png"
    });
    setup
        .send(
            DINA,
            json!({ "to": ["a@one.test"], "attachments": [foreign.clone()] }),
        )
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    setup
        .send(
            OLGA,
            json!({ "to": ["a@one.test"], "attachments": [foreign] }),
        )
        .await
        .assert_status_ok();

    let token = attachment_token::sign(SECRET, url, 107);
    setup
        .send(
            DINA,
            json!({ "to": ["a@one.test"], "attachments": [attachment(Some(token))] }),
        )
        .await
        .assert_status_ok();
    let sent = setup.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].attachments[0].url, url);
    assert_eq!(sent[1].attachments[0].content_type, "application/pdf");
    let stored: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT ea.content_type, ea.content_subtype, ea.filename FROM email_attachments ea
         JOIN emails e ON e.id = ea.email_id WHERE e.sender_user_id = 107",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        stored,
        [("application".into(), "pdf".into(), "quote.pdf".into())]
    );
}

#[test_db::test(seed)]
async fn invalid_requests_are_refused_before_sending(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Sent).await;
    for (body, error) in [
        (
            json!({ "to": [], "body": "Hi" }),
            "Add at least one recipient",
        ),
        (
            json!({ "to": ["nope"], "body": "Hi" }),
            "Invalid email address: nope",
        ),
        (
            json!({ "to": ["a@one.test"], "body": "<p><br></p>" }),
            "Add a message or an attachment",
        ),
        (
            json!({ "to": ["a@one.test"], "body": "Hi", "deal_id": 9999 }),
            "Deal not found",
        ),
        (
            json!({ "to": ["a@one.test"], "body": "Hi", "send_at": "2020-01-01T00:00:00Z" }),
            "Choose a future time",
        ),
    ] {
        let res = setup.send(DINA, body).await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], error);
    }
    // Another company's deal is not found either.
    setup
        .send(
            OLGA,
            json!({ "to": ["a@one.test"], "body": "Hi", "deal_id": 8600 }),
        )
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    assert!(setup.sent().is_empty());
    setup
        .send(
            sessions::VIEWER,
            json!({ "to": ["a@one.test"], "body": "Hi" }),
        )
        .await
        .assert_status(StatusCode::FORBIDDEN);
    setup
        .server
        .post("/v1/emails")
        .json(&json!({ "to": ["a@one.test"], "body": "Hi" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn send_failures_are_reported_and_nothing_is_recorded(pool: MySqlPool) {
    reset_ids(&pool).await;
    let rejected = setup(&pool, Reply::Rejected("Invalid email address")).await;
    let res = rejected
        .send(DINA, json!({ "to": ["a@one.test"], "body": "Hi" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "Invalid email address");

    let down = setup(&pool, Reply::Down).await;
    let res = down
        .send(DINA, json!({ "to": ["a@one.test"], "body": "Hi" }))
        .await;
    res.assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(res.json::<Value>()["error"], "Email failed to send");
    assert!(recorded(&pool, 107).await.is_empty());
}

#[test_db::test(seed)]
async fn scheduled_email_is_stored_for_the_cron(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Sent).await;
    let send_at = (chrono::Utc::now() + chrono::Duration::days(1))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let token = attachment_token::sign(SECRET, "https://files.test/bucket/emails/a.pdf", 107);
    let res = setup
        .send(
            DINA,
            json!({
                "to": ["Nina@example.com"],
                "subject": "Later",
                "body": "Hi",
                "deal_id": 8600,
                "customer_id": 8695,
                "template_id": 8670,
                "send_at": send_at,
                "cancel_if_customer_messages": true,
                "attachments": [{
                    "url": "https://files.test/bucket/emails/a.pdf",
                    "filename": "a.pdf",
                    "content_type": "application/pdf",
                    "token": token,
                }],
            }),
        )
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["delayed"], true);
    assert!(setup.sent().is_empty());
    let (user_id, customer_id, recipients, cancel, status, payload): (
        i32,
        Option<i32>,
        String,
        bool,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT user_id, customer_id, recipient_emails, cancel_if_customer_messages, status, payload_json
         FROM delayed_emails",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((user_id, customer_id), (107, Some(8695)));
    assert_eq!(recipients, r#"["nina@example.com"]"#);
    assert!(cancel);
    assert_eq!(status, "pending");
    let payload: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["to"], json!(["nina@example.com"]));
    assert_eq!(payload["dealId"], "8600");
    assert_eq!(payload["customerId"], "8695");
    assert_eq!(payload["templateId"], "8670");
    assert_eq!(payload["sendTogether"], false);
    let uploaded: Value =
        serde_json::from_str(payload["uploadedAttachments"].as_str().unwrap()).unwrap();
    assert_eq!(
        uploaded,
        json!([{
            "url": "https://files.test/bucket/emails/a.pdf",
            "filename": "a.pdf",
            "contentType": "application",
            "contentSubtype": "pdf",
            "token": token,
        }])
    );
}

#[test_db::test(seed)]
async fn template_use_is_counted(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Sent).await;
    for _ in 0..2 {
        setup
            .send(
                DINA,
                json!({ "to": ["a@one.test"], "body": "Hi", "template_id": 8670 }),
            )
            .await
            .assert_status_ok();
    }
    let count: i32 = sqlx::query_scalar(
        "SELECT usage_count FROM user_template_usage WHERE user_id = 107 AND template_id = 8670",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 2);
}

#[test_db::test(seed)]
async fn email_uploads_carry_a_token(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Sent).await;
    let res = setup
        .server
        .post("/v1/uploads")
        .with_session(DINA)
        .json(&json!({ "folder": "emails", "file_name": "Plan.dwg", "content_type": "application/acad" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let upload: Value = res.json();
    let url = upload["url"].as_str().unwrap();
    assert!(url.starts_with("https://files.test/bucket/emails/"));
    assert!(url.ends_with(".dwg"));
    assert_eq!(upload["token"], attachment_token::sign(SECRET, url, 107));

    let deal = setup
        .server
        .post("/v1/uploads")
        .with_session(DINA)
        .json(&json!({ "folder": "deals", "file_name": "a.png", "content_type": "image/png" }))
        .await;
    assert!(deal.json::<Value>().get("token").is_none());
}

/// Mail leaves from the user's own address only when its domain is the
/// company's mail domain (or a subdomain); any other address, notably one
/// that merely contains the domain or is on another company's verified
/// domain, goes out from the shared address.
#[test_db::test(seed)]
async fn the_from_address_is_the_users_only_on_the_companys_domain(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Sent).await;
    let mut expected = Vec::new();
    for (domain, email, from) in [
        (Some("acme.test"), "dina@acme.test", "dina@acme.test"),
        (
            Some("acme.test"),
            "dina@mail.acme.test",
            "dina@mail.acme.test",
        ),
        (Some("ACME.test"), "dina@acme.TEST", "dina@acme.TEST"),
        (Some("@acme.test"), "dina@acme.test", "dina@acme.test"),
        (Some(" acme.test "), "dina@acme.test", "dina@acme.test"),
        // Company 101's verified domain.
        (
            Some("acme.test"),
            "dina@other.test",
            "sales@granite-manager.com",
        ),
        (
            Some("acme.test"),
            "acme.test@other.test",
            "sales@granite-manager.com",
        ),
        (
            Some("acme.test"),
            "dina@acme.test.other.test",
            "sales@granite-manager.com",
        ),
        (
            Some("acme.test"),
            "dina@notacme.test",
            "sales@granite-manager.com",
        ),
        (
            Some("acme.test"),
            "dina@acme.tes",
            "sales@granite-manager.com",
        ),
        (
            Some("other.test"),
            "dina@acme.test",
            "sales@granite-manager.com",
        ),
        (Some(""), "dina@acme.test", "sales@granite-manager.com"),
        (None, "dina@acme.test", "sales@granite-manager.com"),
    ] {
        sqlx::query("UPDATE company SET domain = ? WHERE id = 100")
            .bind(domain)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET email = ? WHERE id = 107")
            .bind(email)
            .execute(&pool)
            .await
            .unwrap();
        setup
            .send(
                DINA,
                json!({ "to": ["a@one.test"], "subject": email, "body": "<p>Hi</p>" }),
            )
            .await
            .assert_status_ok();
        expected.push((email, from));
    }
    let sent = setup.sent();
    assert_eq!(sent.len(), expected.len());
    for (email, (subject, from)) in sent.iter().zip(&expected) {
        assert_eq!(email.subject, *subject);
        assert_eq!(email.from_address, *from, "{subject}");
    }
    let recorded: Vec<(String, String)> = sqlx::query_as(
        "SELECT subject, sender_email FROM emails WHERE sender_user_id = 107 AND is_draft = 0
          AND subject IS NOT NULL AND receiver_email = 'a@one.test' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        recorded,
        expected
            .iter()
            .map(|(email, from)| ((*email).to_string(), (*from).to_string()))
            .collect::<Vec<_>>()
    );
}

/// A draft's files go out when it is sent, without their upload tokens:
/// the draft is the company's mail. Saving it checked them.
#[test_db::test(seed)]
async fn a_reopened_draft_sends_its_files(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Sent).await;
    let url = "https://files.test/bucket/emails/100/plan.pdf";
    let file = |token: Option<String>| json!({ "url": url, "filename": "plan.pdf", "content_type": "application/pdf", "token": token });
    setup
        .server
        .post("/v1/emails/drafts")
        .with_session(DINA)
        .json(&json!({ "subject": "Plan", "attachments": [file(Some(attachment_token::sign(SECRET, url, 107)))] }))
        .await
        .assert_status(StatusCode::CREATED);
    setup
        .send(
            DINA,
            json!({ "to": ["a@one.test"], "subject": "Plan", "attachments": [file(None)] }),
        )
        .await
        .assert_status_ok();
    assert_eq!(setup.sent()[0].attachments[0].url, url);
    // Not Olga's: it is not her company's mail.
    setup
        .send(
            OLGA,
            json!({ "to": ["a@one.test"], "attachments": [file(None)] }),
        )
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}
