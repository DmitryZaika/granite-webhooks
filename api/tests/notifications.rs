mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::json;
use sqlx::MySqlPool;

// Rita (100, rep@acme.test) has two activity notifications, a billing one,
// and unread mail from Alice in threads t1 and t2; Mark (101) has one too.
const FIXTURES: &str = "
    INSERT INTO notifications (id, user_id, message, due_at) VALUES
      (9701, 100, 'Call Alice', NOW()), (9702, 100, 'Send quote', NOW()), (9703, 101, 'Mark''s', NOW());
    INSERT INTO billing_notifications (id, company_id, user_id, dedupe_key, title, message, action_label, href)
      VALUES (9711, 100, 100, 'card', 'Card', 'Expired', 'Fix', '/billing');
    INSERT INTO emails (id, thread_id, subject, body, sender_email, receiver_email, receiver_user_id, sent_at, company_id, message_id) VALUES
      (9721, 't1', 'Hi', 'x', 'alice@example.com', 'Rita <rep@acme.test>', NULL, NOW(), 100, '<n1@x>'),
      (9722, 't2', 'Hi', 'x', 'alice@example.com', 'rep@acme.test', NULL, NOW(), 100, '<n2@x>'),
      (9723, 't3', 'Hi', 'x', 'alice@example.com', 'manager@acme.test', 101, NOW(), 100, '<n3@x>');";

async fn done_ids(pool: &MySqlPool) -> (Vec<i64>, Vec<i64>) {
    let notifications = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM notifications WHERE id >= 9700 AND is_done = 1 ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let read = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM emails WHERE id >= 9720 AND employee_read_at IS NOT NULL ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    (notifications, read)
}

#[test_db::test(seed)]
async fn listed_notifications_are_dismissed(pool: MySqlPool) {
    sqlx::raw_sql(FIXTURES).execute(&pool).await.unwrap();
    let server = server(pool.clone());
    server
        .post("/v1/notifications/done")
        .with_session(sessions::REP)
        .json(&json!({ "items": [
            { "id": "notif-9701", "kind": "activity" },
            { "id": "notif-9703", "kind": "activity" },
            { "id": "t1", "kind": "email" },
            { "id": "t3", "kind": "email" },
        ] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(done_ids(&pool).await, (vec![9701], vec![9721]));
    server
        .post("/v1/notifications/done")
        .with_session(sessions::REP)
        .json(&json!({}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn clear_all_dismisses_only_the_callers(pool: MySqlPool) {
    sqlx::raw_sql(FIXTURES).execute(&pool).await.unwrap();
    server(pool.clone())
        .post("/v1/notifications/done")
        .with_session(sessions::REP)
        .json(&json!({ "clear_all": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(done_ids(&pool).await, (vec![9701, 9702], vec![9721, 9722]));
    let billing: i8 =
        sqlx::query_scalar("SELECT is_done FROM billing_notifications WHERE id = 9711")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(billing, 1);
}

#[test_db::test(seed)]
async fn the_bell_lists_mail_activity_and_billing(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO notifications (id, user_id, customer_id, message, due_at, notification_type, actor_name, created_at) VALUES
           (9731, 100, 1000, 'Alice opened the contract', NOW() - INTERVAL 1 HOUR, 'signature_viewed', NULL, NOW() - INTERVAL 1 HOUR),
           (9732, 100, 1000, 'Sample', NOW() - INTERVAL 1 HOUR, 'signature_viewed', 'Onboarding Guide', NOW() - INTERVAL 2 HOUR),
           (9733, 101, 1000, 'Not Rita''s', NOW(), 'signature_viewed', NULL, NOW());
         INSERT INTO billing_notifications (id, company_id, user_id, dedupe_key, title, message, action_label, href, created_at)
           VALUES (9741, 100, 100, 'card', 'Card expired', 'Update it', 'Fix', '/billing', NOW() - INTERVAL 3 HOUR);
         INSERT INTO emails (id, thread_id, subject, body, sender_email, receiver_email, sent_at, company_id, message_id) VALUES
           (9751, 'bell-1', 'Question', 'x', 'alice@example.com', 'rep@acme.test', NOW() - INTERVAL 30 MINUTE, 100, '<bell1@x>'),
           (9752, 'onboarding-sample-thread-100', 'Sample', 'x', 'alice@example.com', 'rep@acme.test', NOW() - INTERVAL 20 MINUTE, 100, '<bell2@x>');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let body: serde_json::Value = server
        .get("/v1/notifications")
        .with_session(sessions::REP)
        .await
        .json();
    let items = body["notifications"].as_array().unwrap();
    let ids: Vec<&str> = items
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["bell-1", "notif-9731", "billing-9741"]);
    assert_eq!(items[0]["href"], "/employee/emails/chat/bell-1");
    assert_eq!(items[1]["type_title"], "Document Opened for Signing");
    assert_eq!(items[1]["href"], "/employee/customers/info/1000/documents");
    assert_eq!(items[2]["action_label"], "Fix");
    let with_samples: serde_json::Value = server
        .get("/v1/notifications?samples=true")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(with_samples["notifications"].as_array().unwrap().len(), 5);
}
