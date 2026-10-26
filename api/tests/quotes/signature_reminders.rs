//! `POST /v1/jobs/signature-reminders`: native e-signature expiry, reminders
//! and notifications (the CRM's `nativeSignatureReminders.mysql.test.ts`).

use crate::common;

use api::app;
use api::native_signing::{MailReceipt, MailSnapshot, NativeSigning, sha256_hex};
use api::routes::jobs::signature_reminders::{process_due_reminders, process_expired};
use api::state::AppState;
use api::storage::{MEMORY_PUBLIC_BASE, Storage};
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use chrono::{DateTime, Duration, Utc};
use common::{ALLOWED_ORIGIN, SECRET};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

const INVITATION_SECRET: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
const ORIGIN: &str = "https://sign.example.com";
const DOC: &str = "6f1c2a8e-4b3d-4e5f-9a7b-1c2d3e4f5a6b";
const SENT_AT: &str = "2026-10-01T15:00:00Z";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reply {
    Sent,
    Rejected,
    Down,
}

#[derive(Clone)]
struct Mail {
    sent: Arc<Mutex<Vec<OutgoingEmail>>>,
    reply: Arc<Mutex<Reply>>,
}

async fn fake_send(
    State(mail): State<Mail>,
    Json(email): Json<OutgoingEmail>,
) -> (StatusCode, Json<Value>) {
    mail.sent.lock().unwrap().push(email);
    match *mail.reply.lock().unwrap() {
        Reply::Sent => (
            StatusCode::OK,
            Json(json!({ "message_id": "reminder@example.com" })),
        ),
        Reply::Rejected => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Mailbox busy" })),
        ),
        Reply::Down => (StatusCode::BAD_GATEWAY, Json(json!({ "error": "timeout" }))),
    }
}

struct Setup {
    state: AppState,
    signing: NativeSigning,
    mail: Mail,
    storage: Storage,
    pool: MySqlPool,
}

fn signing() -> NativeSigning {
    NativeSigning::new(INVITATION_SECRET, ORIGIN).unwrap()
}

fn sent_at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(SENT_AT).unwrap().to_utc()
}

/// `n` days after sending, plus a second.
fn day(n: i64) -> DateTime<Utc> {
    sent_at() + Duration::days(n) + Duration::seconds(1)
}

fn settings() -> Value {
    json!({
        "parityVerified": true, "timezone": "America/New_York", "signatureMethods": ["type", "draw"],
        "authentication": "invitation", "allowDecline": true, "allowReassign": false, "reminders": true,
        "expiresInDays": null, "notifications": { "viewed": false, "completed": false, "declined": false },
        "auditPage": true
    })
}

/// Request 9301 sent by Rita (user 100) to Ada, with its invitation mail.
async fn setup(pool: MySqlPool) -> Setup {
    let mail = Mail {
        sent: Arc::default(),
        reply: Arc::new(Mutex::new(Reply::Sent)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/email/send", post(fake_send))
        .with_state(mail.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let storage = Storage::memory();
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_storage(storage.clone())
        .with_signature_bucket("signatures-private")
        .with_webhooks(WebhooksClient::new(
            Some(&format!("http://{address}")),
            Some("key"),
        ))
        .with_native_signing(Some(signing()));
    sqlx::query(
        "INSERT INTO signature_requests (id, company_id, customer_id, created_by_user_id, filename, file_url,
             signer_name, signer_email, status, provider, provider_document_id)
         VALUES (9301, 100, 1000, 100, 'contract.pdf', 's3://fixture/original.pdf', 'Ada', 'ada@example.com',
             'sent', 'native', ?)",
    )
    .bind(DOC)
    .execute(&pool)
    .await
    .unwrap();
    let logical = "b".repeat(64);
    let token = signing().token(100, 100, &logical, DOC);
    sqlx::query(
        "INSERT INTO native_signature_documents (request_id, company_id, document_id, prepared_object_key,
             prepared_sha256, document_version, token_hash, fields_json, settings_json, sent_at)
         VALUES (9301, 100, ?, 'k', ?, ?, ?, '[]', ?, ?)",
    )
    .bind(DOC)
    .bind("c".repeat(64))
    .bind("c".repeat(64))
    .bind(sha256_hex(token.as_bytes()))
    .bind(settings().to_string())
    .bind(sent_at().naive_utc())
    .execute(&pool)
    .await
    .unwrap();
    let receipt = MailReceipt {
        snapshot: MailSnapshot {
            subject: "Contract".into(),
            personalized_body: "Sign".into(),
            thread_id: "fixture".into(),
            from: "Staff <staff@example.com>".into(),
            to: vec!["ada@example.com".into()],
            cc: vec![],
            bcc: vec![],
            html: "Please sign".into(),
            text: "Please sign".into(),
            in_reply_to: None,
            references: vec!["<invite@example.com>".into()],
        },
        message_id: "invite@example.com".into(),
    };
    sqlx::query(
        "INSERT INTO native_signature_deliveries (id, company_id, created_by_user_id, logical_send_hash, payload_sha256,
             state, encrypted_mail) VALUES (9401, 100, 100, ?, ?, 'accepted', ?)",
    )
    .bind(&logical)
    .bind("d".repeat(64))
    .bind(signing().seal(&receipt))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO native_signature_delivery_items (delivery_id, company_id, request_id, attachment_index) VALUES (9401, 100, 9301, 0)")
        .execute(&pool)
        .await
        .unwrap();
    Setup {
        state,
        signing: signing(),
        mail,
        storage,
        pool,
    }
}

impl Setup {
    async fn remind(&self, now: DateTime<Utc>) -> (usize, usize) {
        process_due_reminders(&self.state, &self.signing, now)
            .await
            .unwrap()
    }

    fn sent(&self) -> Vec<OutgoingEmail> {
        self.mail.sent.lock().unwrap().clone()
    }

    fn reply(&self, reply: Reply) {
        *self.mail.reply.lock().unwrap() = reply;
    }

    async fn sql(&self, sql: &str) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&self.pool)
            .await
            .unwrap();
    }

    async fn status(&self) -> String {
        sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM signature_requests WHERE id = 9301")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

#[test_db::test(seed)]
async fn concurrent_runs_send_a_reminder_once(pool: MySqlPool) {
    let s = setup(pool).await;
    let (a, b) = tokio::join!(s.remind(day(3)), s.remind(day(3)));
    assert_eq!(a.0 + b.0, 1);
    let sent = s.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, ["ada@example.com"]);
    assert_eq!(sent[0].subject, "Signature reminder: contract.pdf");
    assert_eq!(sent[0].from_address, "staff@example.com");
    assert_eq!(sent[0].from_name.as_deref(), Some("Staff"));
    assert_eq!(sent[0].references, ["<invite@example.com>"]);
    assert!(
        sent[0].body.contains("https://sign.example.com/sign/"),
        "{}",
        sent[0].body
    );
    assert_eq!(s.remind(day(3)).await.0, 0);
}

#[test_db::test(seed)]
async fn reminders_go_out_on_days_3_6_and_10_only(pool: MySqlPool) {
    let s = setup(pool).await;
    for d in [2, 3, 4, 6, 9, 10, 11] {
        s.remind(day(d)).await;
    }
    assert_eq!(s.sent().len(), 3);
}

#[test_db::test(seed)]
async fn finished_requests_get_no_reminders(pool: MySqlPool) {
    let s = setup(pool).await;
    for status in ["signed", "declined", "expired"] {
        s.sql(&format!(
            "UPDATE signature_requests SET status = '{status}' WHERE id = 9301"
        ))
        .await;
        assert_eq!(s.remind(day(10)).await.0, 0);
    }
    assert!(s.sent().is_empty());
}

#[test_db::test(seed)]
async fn reminders_off_sends_none(pool: MySqlPool) {
    let s = setup(pool).await;
    s.sql("UPDATE native_signature_documents SET settings_json = JSON_SET(settings_json, '$.reminders', CAST('false' AS JSON))").await;
    assert_eq!(s.remind(day(10)).await.0, 0);
}

#[test_db::test(seed)]
async fn a_signed_request_never_expires(pool: MySqlPool) {
    let s = setup(pool).await;
    s.sql(&format!("UPDATE signature_requests SET status = 'signed' WHERE id = 9301; UPDATE native_signature_documents SET expires_at = '{}' WHERE request_id = 9301", sent_at().naive_utc())).await;
    assert_eq!(process_expired(&s.pool, day(1)).await.unwrap(), 0);
    assert_eq!(s.status().await, "signed");
}

#[test_db::test(seed)]
async fn an_open_request_expires_and_its_reminders_stop(pool: MySqlPool) {
    let s = setup(pool).await;
    s.sql(&format!(
        "UPDATE native_signature_documents SET expires_at = '{}' WHERE request_id = 9301",
        sent_at().naive_utc()
    ))
    .await;
    assert_eq!(process_expired(&s.pool, day(1)).await.unwrap(), 1);
    assert_eq!(s.status().await, "expired");
    let audit: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM native_signature_audit WHERE request_id = 9301 AND event_type = 'expired'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(audit, 1);
    assert_eq!(s.remind(day(3)).await.0, 0);
}

#[test_db::test(seed)]
async fn an_unknown_outcome_is_never_retried(pool: MySqlPool) {
    let s = setup(pool).await;
    s.reply(Reply::Down);
    assert_eq!(s.remind(day(3)).await, (0, 1));
    s.remind(day(3)).await;
    s.remind(day(3) + Duration::hours(2)).await;
    assert_eq!(s.sent().len(), 1);
    let state: String = sqlx::query_scalar("SELECT CAST(state AS CHAR) FROM native_signature_jobs")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(state, "uncertain");
}

#[test_db::test(seed)]
async fn refused_sends_are_retried_three_times_at_most(pool: MySqlPool) {
    let s = setup(pool).await;
    s.reply(Reply::Rejected);
    for hour in 0..5 {
        s.remind(day(3) + Duration::hours(hour)).await;
    }
    assert_eq!(s.sent().len(), 3);
}

#[test_db::test(seed)]
async fn after_an_outage_only_the_latest_reminder_goes_out(pool: MySqlPool) {
    let s = setup(pool).await;
    s.remind(day(10)).await;
    assert_eq!(s.sent().len(), 1);
}

#[test_db::test(seed)]
async fn a_viewed_notification_goes_to_its_frozen_recipients_once(pool: MySqlPool) {
    let s = setup(pool).await;
    let mut frozen = settings();
    frozen["reminders"] = json!(false);
    frozen["notifications"]["viewed"] = json!(true);
    frozen["notificationDelivery"] = json!({ "viewed": {
        "recipients": ["sender"], "subject": "Viewed {{filename}}", "text": "Customer viewed {{filename}}", "completedFile": "none"
    }});
    sqlx::query("UPDATE native_signature_documents SET settings_json = ? WHERE request_id = 9301")
        .bind(frozen.to_string())
        .execute(&s.pool)
        .await
        .unwrap();
    s.sql(&format!(
        "UPDATE signature_requests SET status = 'viewed' WHERE id = 9301;
         INSERT INTO native_signature_audit (request_id, company_id, event_key, event_type, evidence_json, created_at)
         VALUES (9301, 100, 'viewed', 'viewed', '{{}}', '{}')",
        sent_at().naive_utc()
    ))
    .await;
    assert_eq!(s.remind(day(1)).await.0, 1);
    let sent = s.sent();
    assert_eq!(sent[0].to, ["staff@example.com"]);
    assert_eq!(sent[0].subject, "Viewed contract.pdf");
    assert_eq!(sent[0].body, "Customer viewed contract.pdf");
    s.remind(day(1)).await;
    assert_eq!(s.sent().len(), 1);
}

#[test_db::test(seed)]
async fn a_completed_notification_attaches_the_intact_signed_file(pool: MySqlPool) {
    let s = setup(pool).await;
    let signed = b"%PDF-1.7 signed".to_vec();
    let hash = sha256_hex(&signed);
    let key = format!("100/signatures/{DOC}/completed-{hash}.pdf");
    s.storage
        .put(&key, signed, "application/pdf")
        .await
        .unwrap();
    let mut frozen = settings();
    frozen["notifications"]["completed"] = json!(true);
    frozen["notificationDelivery"] = json!({ "completed": {
        "recipients": ["sender", "signer"], "subject": "Signed {{filename}}", "text": "Done", "completedFile": "attachment"
    }});
    sqlx::query("UPDATE native_signature_documents SET settings_json = ?, completed_object_key = ?, completed_sha256 = ? WHERE request_id = 9301")
        .bind(frozen.to_string())
        .bind(&key)
        .bind(&hash)
        .execute(&s.pool)
        .await
        .unwrap();
    s.sql(&format!(
        "UPDATE signature_requests SET status = 'signed' WHERE id = 9301;
         INSERT INTO native_signature_audit (request_id, company_id, event_key, event_type, evidence_json, created_at)
         VALUES (9301, 100, 'completed', 'completed', '{{}}', '{}')",
        sent_at().naive_utc()
    ))
    .await;
    assert_eq!(s.remind(day(1)).await.0, 1);
    let sent = s.sent();
    assert_eq!(sent[0].to, ["staff@example.com", "ada@example.com"]);
    assert_eq!(sent[0].attachments.len(), 1);
    assert_eq!(
        sent[0].attachments[0].url,
        format!("{MEMORY_PUBLIC_BASE}/{key}")
    );
    assert_eq!(sent[0].attachments[0].filename, "contract.pdf");
}

#[test_db::test(seed)]
async fn a_tampered_signed_file_blocks_the_notification(pool: MySqlPool) {
    let s = setup(pool).await;
    let hash = "e".repeat(64);
    let key = format!("100/signatures/{DOC}/completed-{hash}.pdf");
    s.storage
        .put(&key, b"%PDF-1.7 other".to_vec(), "application/pdf")
        .await
        .unwrap();
    let mut frozen = settings();
    frozen["notifications"]["completed"] = json!(true);
    frozen["notificationDelivery"] = json!({ "completed": {
        "recipients": ["sender"], "subject": "Signed", "text": "Done", "completedFile": "attachment"
    }});
    sqlx::query("UPDATE native_signature_documents SET settings_json = ?, completed_object_key = ?, completed_sha256 = ? WHERE request_id = 9301")
        .bind(frozen.to_string())
        .bind(&key)
        .bind(&hash)
        .execute(&s.pool)
        .await
        .unwrap();
    s.sql(&format!(
        "UPDATE signature_requests SET status = 'signed' WHERE id = 9301;
         INSERT INTO native_signature_audit (request_id, company_id, event_key, event_type, evidence_json, created_at)
         VALUES (9301, 100, 'completed', 'completed', '{{}}', '{}')",
        sent_at().naive_utc()
    ))
    .await;
    assert_eq!(s.remind(day(1)).await, (0, 1));
    assert!(s.sent().is_empty());
    let state: String = sqlx::query_scalar("SELECT CAST(state AS CHAR) FROM native_signature_jobs")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(state, "blocked");
}

#[test_db::test(seed)]
async fn an_accepted_signature_neither_expires_nor_gets_reminders(pool: MySqlPool) {
    let s = setup(pool).await;
    s.sql(&format!(
        "INSERT INTO native_signature_finalizations (request_id, company_id, idempotency_key, submission_sha256,
             submission_json, audit_json, signed_at, state)
         VALUES (9301, 100, 'k', '{}', '{{}}', '{{}}', '{at}', 'retryable');
         UPDATE native_signature_documents SET expires_at = '{at}' WHERE request_id = 9301",
        "a".repeat(64),
        at = sent_at().naive_utc()
    ))
    .await;
    assert_eq!(process_expired(&s.pool, day(1)).await.unwrap(), 0);
    assert_eq!(s.remind(day(3)).await.0, 0);
}

#[test_db::test(seed)]
async fn accepted_signatures_do_not_starve_later_expiries(pool: MySqlPool) {
    let s = setup(pool).await;
    let at = sent_at().naive_utc();
    for i in 0..101 {
        let id = 9500 + i;
        let document = format!("00000000-0000-4000-8000-{:012}", i);
        s.sql(&format!(
            "INSERT INTO signature_requests (id, company_id, created_by_user_id, filename, file_url, signer_name,
                 signer_email, status, provider, provider_document_id)
             VALUES ({id}, 100, 100, 'f.pdf', 's3://f', 'Ada', 'ada@example.com', 'sent', 'native', '{document}');
             INSERT INTO native_signature_documents (request_id, company_id, document_id, prepared_object_key,
                 prepared_sha256, document_version, token_hash, fields_json, settings_json, sent_at, expires_at)
             VALUES ({id}, 100, '{document}', 'k', '{h}', '{h}', '{t:064}', '[]', '{{}}', '{at}', '{at}')",
            h = "c".repeat(64),
            t = i,
        ))
        .await;
        if i < 100 {
            s.sql(&format!(
                "INSERT INTO native_signature_finalizations (request_id, company_id, idempotency_key, submission_sha256,
                     submission_json, audit_json, signed_at, state)
                 VALUES ({id}, 100, 'k{i}', '{}', '{{}}', '{{}}', '{at}', 'retryable')",
                "a".repeat(64)
            ))
            .await;
        }
    }
    assert_eq!(process_expired(&s.pool, day(1)).await.unwrap(), 1);
}

#[test_db::test(seed)]
async fn the_job_needs_its_key_and_idles_while_disabled(pool: MySqlPool) {
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN).with_job_key("job-key");
    let server = TestServer::new(app(state));
    server
        .post("/v1/jobs/signature-reminders")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    let body: Value = server
        .post("/v1/jobs/signature-reminders")
        .add_header("Authorization", "job-key")
        .await
        .json();
    assert_eq!(
        body,
        json!({ "processed": 0, "sent": 0, "failed": 0, "expired": 0 })
    );
}
