use crate::common;

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
        "INSERT INTO notifications (id, user_id, deal_id, customer_id, message, due_at, notification_type, actor_name, created_at) VALUES
           (9731, 100, 8000, NULL, 'Your text went out', NOW() - INTERVAL 1 HOUR, 'delayed_message_sent', NULL, NOW() - INTERVAL 1 HOUR),
           (9732, 100, 8000, NULL, 'Sample', NOW() - INTERVAL 1 HOUR, 'note_added', 'Onboarding Guide', NOW() - INTERVAL 2 HOUR),
           (9733, 101, 8000, NULL, 'Not Rita''s', NOW(), 'note_added', NULL, NOW()),
           -- Signature status is emailed to the sender instead.
           (9734, 100, NULL, 1000, 'Alice opened the contract', NOW() - INTERVAL 1 HOUR, 'signature_viewed', NULL, NOW() - INTERVAL 1 HOUR),
           (9735, 100, 8000, NULL, 'Alice signed', NOW() - INTERVAL 1 HOUR, 'signature_signed', NULL, NOW() - INTERVAL 1 HOUR);
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
    assert_eq!(items[1]["type_title"], "Scheduled Message Sent");
    assert_eq!(items[1]["href"], "/employee/deals/edit/8000/project");
    assert_eq!(items[2]["action_label"], "Fix");
    let with_samples: serde_json::Value = server
        .get("/v1/notifications?samples=true")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(with_samples["notifications"].as_array().unwrap().len(), 5);
}

async fn listed_ids(server: &axum_test::TestServer) -> Vec<String> {
    let body: serde_json::Value = server
        .get("/v1/notifications")
        .with_session(sessions::REP)
        .await
        .json();
    body["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_string())
        .collect()
}

#[test_db::test(seed)]
async fn hidden_mail_stays_unread_until_a_newer_message(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO emails (id, thread_id, subject, body, sender_email, receiver_email, sent_at, company_id, message_id) VALUES
           (9761, 'hide-1', 'Hi', 'x', 'alice@example.com', 'rep@acme.test', NOW() - INTERVAL 1 HOUR, 100, '<hide1@x>'),
           (9762, 'hide-2', 'Hi', 'x', 'alice@example.com', 'rep@acme.test', NOW() - INTERVAL 1 HOUR, 100, '<hide2@x>');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());
    server
        .post("/v1/notifications/done")
        .with_session(sessions::REP)
        .json(&json!({ "hide_only": true, "items": [{ "id": "hide-1", "kind": "email" }] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(listed_ids(&server).await, ["hide-2"]);
    assert_eq!(done_ids(&pool).await.1, Vec::<i64>::new());

    // The hide happened 45 minutes ago; a reply came after it.
    sqlx::raw_sql(
        "UPDATE notification_email_hides SET hidden_at = NOW() - INTERVAL 45 MINUTE;
         INSERT INTO emails (id, thread_id, subject, body, sender_email, receiver_email, sent_at, company_id, message_id) VALUES
           (9763, 'hide-1', 'Re: Hi', 'x', 'alice@example.com', 'rep@acme.test', NOW() - INTERVAL 30 MINUTE, 100, '<hide3@x>');",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(listed_ids(&server).await, ["hide-1", "hide-2"]);

    server
        .post("/v1/notifications/done")
        .with_session(sessions::REP)
        .json(&json!({ "hide_only": true, "clear_all": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(listed_ids(&server).await, Vec::<String>::new());
    assert_eq!(done_ids(&pool).await.1, Vec::<i64>::new());
}

/// Hiding everything on the Lambda's one-connection pool must not wait for a
/// second connection while the transaction holds the only one.
#[test_db::test(seed)]
async fn hiding_all_mail_works_on_the_lambda_pool(pool: MySqlPool) {
    sqlx::raw_sql(FIXTURES).execute(&pool).await.unwrap();
    let lambda = api::db::lambda_pool((*pool.connect_options()).clone());
    server(lambda)
        .post("/v1/notifications/done")
        .with_session(sessions::REP)
        .json(&json!({ "hide_only": true, "clear_all": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let mut hidden: Vec<String> =
        sqlx::query_scalar("SELECT thread_id FROM notification_email_hides WHERE user_id = 100")
            .fetch_all(&pool)
            .await
            .unwrap();
    hidden.sort();
    assert_eq!(hidden, ["t1", "t2"]);
}

#[test_db::test(seed)]
async fn deadline_reminders_stay_until_dismissed(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO deal_activities (deal_id, company_id, name, deadline, is_completed)
           VALUES (8000, 100, 'Site visit', '2026-05-01 14:30:00', 0);
         INSERT INTO notifications (id, user_id, deal_id, message, due_at, notification_type, created_at)
           VALUES (9771, 100, 8000, 'Site visit', UTC_TIMESTAMP() - INTERVAL 2 HOUR, 'activity_deadline_reminder', NOW() - INTERVAL 3 DAY);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());
    assert_eq!(listed_ids(&server).await, ["notif-9771"]);
    sqlx::query("UPDATE deal_activities SET is_completed = 1 WHERE deal_id = 8000")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(listed_ids(&server).await, Vec::<String>::new());
}

/// Mail reaches the bell by the user's exact address, and only their
/// company's: an address that is a SQL pattern (`%@%.%`) matched every
/// company's mail, and clearing marked it read for them.
#[test_db::test(seed)]
async fn other_companies_mail_never_reaches_the_bell(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO emails (id, thread_id, subject, body, sender_email, receiver_email, sent_at, company_id, message_id) VALUES
           (9761, 'other-1', 'Their deal', 'x', 'bob@example.com', 'Olga <rep@other.test>', NOW(), 101, '<o1@x>'),
           (9762, 'other-2', 'Their deal', 'x', 'bob@example.com', 'rep@acme.test', NOW(), 101, '<o2@x>'),
           (9763, 'own-1', 'Mine', 'x', 'alice@example.com', 'rep@acme.test', NOW(), 100, '<o3@x>');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());
    assert_eq!(listed_ids(&server).await, ["own-1"]);

    sqlx::query("UPDATE users SET email = '%@%.%' WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    assert!(listed_ids(&server).await.is_empty());
    server
        .post("/v1/notifications/done")
        .with_session(sessions::REP)
        .json(&json!({ "clear_all": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let read: Vec<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM emails WHERE id IN (9761, 9762, 9763) AND employee_read_at IS NOT NULL",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(read.is_empty(), "{read:?}");
}

// Who reaches Rita's (100, rep@acme.test, company 100) bell. Mark (101) is
// her coworker; Olga (106) is of company 101. Each thread id names its case.
const ADDRESSED: &str = "
    INSERT INTO emails (id, thread_id, subject, body, sender_email, sender_user_id, receiver_email,
                        receiver_user_id, deal_id, sent_at, employee_read_at, deleted_at, company_id, message_id) VALUES
      (9801, 'bare',          's', 'x', 'alice@example.com', NULL, 'rep@acme.test',                    NULL, NULL, NOW(), NULL, NULL, 100,  '<a1@x>'),
      (9802, 'named',         's', 'x', 'alice@example.com', NULL, 'Rita <rep@acme.test>',             NULL, NULL, NOW(), NULL, NULL, 100,  '<a2@x>'),
      (9803, 'cased',         's', 'x', 'alice@example.com', NULL, '  REP@Acme.Test ',                 NULL, NULL, NOW(), NULL, NULL, 100,  '<a3@x>'),
      (9804, 'named-cased',   's', 'x', 'alice@example.com', NULL, 'Rita < Rep@ACME.test >',           NULL, NULL, NOW(), NULL, NULL, 100,  '<a4@x>'),
      (9805, 'list-last',     's', 'x', 'alice@example.com', NULL, 'Bob <b@x.test>, Rita <rep@acme.test>', NULL, NULL, NOW(), NULL, NULL, 100, '<a5@x>'),
      (9806, 'by-id',         's', 'x', 'alice@example.com', NULL, 'someone@else.test',                100,  NULL, NOW(), NULL, NULL, 100,  '<a6@x>'),
      (9807, 'her-deal',      's', 'x', 'alice@example.com', NULL, 'sales@acme.test',                  NULL, 8000, NOW(), NULL, NULL, 100,  '<a7@x>'),
      (9808, 'coworker',      's', 'x', 'manager@acme.test', 101,  'rep@acme.test',                    NULL, NULL, NOW(), NULL, NULL, NULL, '<a8@x>'),
      (9811, 'other-co',      's', 'x', 'bob@example.com',   NULL, 'rep@acme.test',                    NULL, NULL, NOW(), NULL, NULL, 101,  '<b1@x>'),
      (9812, 'other-named',   's', 'x', 'bob@example.com',   NULL, 'Rita <rep@acme.test>',             NULL, NULL, NOW(), NULL, NULL, 101,  '<b2@x>'),
      (9813, 'other-coworker','s', 'x', 'rep@other.test',    106,  'rep@acme.test',                    NULL, NULL, NOW(), NULL, NULL, NULL, '<b3@x>'),
      (9814, 'unstamped',     's', 'x', 'bob@example.com',   NULL, 'rep@acme.test',                    NULL, NULL, NOW(), NULL, NULL, NULL, '<b4@x>'),
      (9815, 'longer-local',  's', 'x', 'alice@example.com', NULL, 'Rita <xrep@acme.test>',            NULL, NULL, NOW(), NULL, NULL, 100,  '<b5@x>'),
      (9816, 'longer-domain', 's', 'x', 'alice@example.com', NULL, 'rep@acme.test.evil',               NULL, NULL, NOW(), NULL, NULL, 100,  '<b6@x>'),
      (9817, 'list-first',    's', 'x', 'alice@example.com', NULL, 'Rita <rep@acme.test>, Bob <b@x.test>', NULL, NULL, NOW(), NULL, NULL, 100, '<b7@x>'),
      (9818, 'her-own',       's', 'x', 'rep@acme.test',     100,  'alice@example.com',                NULL, NULL, NOW(), NULL, NULL, 100,  '<b8@x>'),
      (9819, 'read',          's', 'x', 'alice@example.com', NULL, 'rep@acme.test',                    NULL, NULL, NOW(), NOW(), NULL, 100, '<b9@x>'),
      (9820, 'old',           's', 'x', 'alice@example.com', NULL, 'rep@acme.test',                    NULL, NULL, NOW() - INTERVAL 15 DAY, NULL, NULL, 100, '<c1@x>'),
      (9821, 'deleted',       's', 'x', 'alice@example.com', NULL, 'rep@acme.test',                    NULL, NULL, NOW(), NULL, NOW(), 100, '<c2@x>');";

/// The threads `ADDRESSED` puts in Rita's bell: her address however it is
/// written (bare, `Name <address>`, any case, the last of a list), mail to
/// her by id or on her deal, and a coworker's legacy mail to her. Never
/// another company's mail, nor mail whose company is unknown.
const RITAS: [&str; 8] = [
    "bare",
    "by-id",
    "cased",
    "coworker",
    "her-deal",
    "list-last",
    "named",
    "named-cased",
];

async fn read_threads(pool: &MySqlPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT thread_id FROM emails WHERE id BETWEEN 9801 AND 9821 AND employee_read_at IS NOT NULL
            AND thread_id <> 'read' ORDER BY thread_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn sorted_ids(server: &axum_test::TestServer) -> Vec<String> {
    let mut ids = listed_ids(server).await;
    ids.sort();
    ids
}

/// The bell lists exactly the user's own mail of their company, and
/// clearing it marks exactly those threads read.
#[test_db::test(seed)]
async fn the_bell_matches_the_users_address_in_their_company(pool: MySqlPool) {
    sqlx::raw_sql(ADDRESSED).execute(&pool).await.unwrap();
    let server = server(pool.clone());
    assert_eq!(sorted_ids(&server).await, RITAS);
    server
        .post("/v1/notifications/done")
        .with_session(sessions::REP)
        .json(&json!({ "clear_all": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // Clearing marks all of her unread mail read, older than the bell's
    // 14 days too, as before.
    let mut cleared = RITAS.to_vec();
    cleared.push("old");
    cleared.sort_unstable();
    assert_eq!(read_threads(&pool).await, cleared);
    assert!(listed_ids(&server).await.is_empty());
}

/// An address that is a SQL pattern matches nothing but mail to the user by
/// id or on their deal, and clearing marks nothing else read (the attack
/// read and cleared every company's mail).
#[test_db::test(seed)]
async fn pattern_addresses_match_no_ones_mail(pool: MySqlPool) {
    sqlx::raw_sql(ADDRESSED).execute(&pool).await.unwrap();
    let server = server(pool.clone());
    for pattern in [
        "%@%.%",
        "%",
        "%rep@acme.test",
        "rep@acme.tes_",
        "_ep@acme.test",
        "%<rep@acme.test>",
    ] {
        sqlx::query("UPDATE users SET email = ? WHERE id = 100")
            .bind(pattern)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            sorted_ids(&server).await,
            ["by-id", "her-deal"],
            "{pattern}"
        );
    }
    server
        .post("/v1/notifications/done")
        .with_session(sessions::REP)
        .json(&json!({ "clear_all": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(read_threads(&pool).await, ["by-id", "her-deal"]);
}

/// Naming another company's thread dismisses nothing: neither marks it
/// read nor hides it for the caller.
#[test_db::test(seed)]
async fn other_companies_threads_cannot_be_dismissed(pool: MySqlPool) {
    sqlx::raw_sql(ADDRESSED).execute(&pool).await.unwrap();
    let server = server(pool.clone());
    let others = json!([
        { "id": "other-co", "kind": "email" },
        { "id": "other-named", "kind": "email" },
        { "id": "other-coworker", "kind": "email" },
        { "id": "unstamped", "kind": "email" },
    ]);
    server
        .post("/v1/notifications/done")
        .with_session(sessions::REP)
        .json(&json!({ "items": others }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(read_threads(&pool).await.is_empty());

    // Hiding everything hides only what the bell listed.
    server
        .post("/v1/notifications/done")
        .with_session(sessions::REP)
        .json(&json!({ "hide_only": true, "clear_all": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let mut hidden: Vec<String> =
        sqlx::query_scalar("SELECT thread_id FROM notification_email_hides WHERE user_id = 100")
            .fetch_all(&pool)
            .await
            .unwrap();
    hidden.sort();
    assert_eq!(hidden, RITAS);
    assert!(read_threads(&pool).await.is_empty());
}

/// The other company's rep gets her own company's mail to her address,
/// never Acme's mail that names her address too.
#[test_db::test(seed)]
async fn each_company_sees_only_its_own_mail_to_an_address(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO emails (id, thread_id, subject, body, sender_email, receiver_email, sent_at, company_id, message_id) VALUES
           (9831, 'olga-own', 's', 'x', 'bob@example.com', 'Olga <rep@other.test>', NOW(), 101, '<d1@x>'),
           (9832, 'acme-to-olga', 's', 'x', 'alice@example.com', 'rep@other.test', NOW(), 100, '<d2@x>');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let body: serde_json::Value = server(pool)
        .get("/v1/notifications")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    let mail: Vec<&str> = body["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "email")
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert!(mail.contains(&"olga-own"), "{mail:?}");
    assert!(!mail.contains(&"acme-to-olga"), "{mail:?}");
}
