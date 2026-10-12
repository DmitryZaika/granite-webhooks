mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::Value;
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn an_admin_reads_an_emails_conversation(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO emails (id, thread_id, subject, body, sender_email, receiver_email, sender_user_id, sent_at, company_id, message_id) VALUES
           (9901, 'conv-1', 'Quote', '<p>Here</p>', 'rep@acme.test', 'alice@example.com', 100, '2026-05-01 10:00:00', 100, '<a@x>'),
           (9902, 'conv-1', 'Re: Quote', 'Thanks', 'alice@example.com', 'rep@acme.test', NULL, '2026-05-01 11:00:00', 100, '<b@x>'),
           (9903, 'conv-2', 'Theirs', 'x', 'olga@other.test', 'z@example.com', 106, '2026-05-01 11:00:00', 101, '<c@x>');
         INSERT INTO email_attachments (email_id, filename, url, content_type, content_subtype)
           VALUES (9902, 'plan.pdf', 'https://files.example/plan.pdf', 'application', 'pdf');
         INSERT INTO email_reads (message_id, read_at) VALUES ('<a@x>', '2026-05-01 10:30:00');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let body: Value = server
        .get("/v1/emails/9902/conversation")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(body["thread_id"], "conv-1");
    assert_eq!(body["truncated"], false);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["id"], 9901);
    assert_eq!(messages[0]["is_from_customer"], false);
    assert_eq!(messages[0]["sender_name"], "Rita Rep");
    assert_eq!(messages[0]["opened_at"], "2026-05-01T10:30:00Z");
    assert_eq!(messages[1]["is_from_customer"], true);
    assert_eq!(messages[1]["attachments"][0]["filename"], "plan.pdf");
    server
        .get("/v1/emails/9903/conversation")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/emails/9902/conversation")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}
