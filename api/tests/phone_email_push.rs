//! The `email` push for a new inbound email (`POST /v1/emails/inbound-push`,
//! called by the `webhooks` Lambda with its key).
//!
//! Seed mailbox: company 101 receives email (domain `other.test`); Olga (106,
//! phone 1) has unread customer mail 8802 in thread T1. Rita (100, phone 2)
//! is of Acme (100), which has no inbound email.

use crate::phone_common;

use axum::http::StatusCode;
use axum_test::TestServer;
use phone_common::{JOB_KEY, Stubs, add_phone, phone_server, sql};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const T1: &str = "aaaaaaaa-1111-4000-8000-000000000001";

async fn setup(pool: &MySqlPool) -> (TestServer, Stubs) {
    let (server, stubs) = phone_server(pool.clone()).await;
    add_phone(pool, 1, 106, 101).await;
    add_phone(pool, 2, 100, 100).await;
    (server, stubs)
}

/// Asks for the push of `email_id`: how many phones were told.
async fn push(server: &TestServer, email_id: i64) -> Value {
    let res = server
        .post("/v1/emails/inbound-push")
        .add_header("authorization", JOB_KEY)
        .json(&json!({ "email_id": email_id }))
        .await;
    res.assert_status_ok();
    res.json()
}

/// The `email` pushes FCM got: (push token, data).
fn email_pushes(stubs: &Stubs) -> Vec<(String, Value)> {
    stubs
        .pushes()
        .into_iter()
        .filter(|push| push["data"]["type"] == "email")
        .map(|push| {
            (
                push["token"].as_str().unwrap().to_string(),
                push["data"].clone(),
            )
        })
        .collect()
}

#[test_db::test(seed)]
async fn a_new_inbound_email_pushes_its_recipients_phone(pool: MySqlPool) {
    let (server, stubs) = setup(&pool).await;
    assert_eq!(push(&server, 8802).await, json!({ "pushed": 1 }));
    // Ids only: no subject, sender or text.
    assert_eq!(
        email_pushes(&stubs),
        [(
            "push-1".to_string(),
            json!({ "granite": "1", "type": "email", "emailId": "8802", "threadId": T1 })
        )]
    );

    // A new customer reply on the thread, addressed to Olga only by header.
    sql(
        &pool,
        "INSERT INTO emails (id, subject, body, sent_at, message_id, thread_id, sender_email,
                             receiver_email, company_id, is_draft)
         VALUES (8890, 'Re: Kitchen quote', 'One more thing', '2026-04-12 09:00:00',
                 '<m8890@other-customer.test>', 'aaaaaaaa-1111-4000-8000-000000000001',
                 'Bea Buyer <buyer@other-customer.test>', 'Olga Other <REP@other.test>', 101, 0)",
    )
    .await;
    assert_eq!(push(&server, 8890).await, json!({ "pushed": 1 }));
    assert_eq!(email_pushes(&stubs)[1].1["emailId"], "8890");
}

#[test_db::test(seed)]
async fn mail_nobody_should_hear_about_is_not_pushed(pool: MySqlPool) {
    let (server, stubs) = setup(&pool).await;
    // Olga's own send to Olga's own address from outside the CRM.
    sql(
        &pool,
        "INSERT INTO emails (id, subject, body, sent_at, message_id, thread_id, receiver_user_id,
                             sender_email, receiver_email, company_id, is_draft)
         VALUES (8891, 'Note to self', 'x', '2026-04-12 09:00:00', '<m8891@other.test>',
                 'aaaaaaaa-1111-4000-8000-000000000009', 106, 'rep@other.test',
                 'rep@other.test', 101, 0)",
    )
    .await;
    // Unknown; Olga's reply (outbound); already read; deleted; a draft;
    // Acme's mail (no inbound email); sent by Olga from her address.
    for email_id in [999_999, 8801, 8800, 8805, 8806, 8807, 8891] {
        assert_eq!(
            push(&server, email_id).await,
            json!({ "pushed": 0 }),
            "{email_id}"
        );
    }
    assert!(email_pushes(&stubs).is_empty());

    // A signed-out phone hears nothing either.
    sql(
        &pool,
        "UPDATE device_sessions SET revoked_at = UTC_TIMESTAMP() WHERE id = 950001",
    )
    .await;
    assert_eq!(push(&server, 8802).await, json!({ "pushed": 0 }));
    assert!(email_pushes(&stubs).is_empty());
}

#[test_db::test(seed)]
async fn the_push_needs_the_key_and_an_email_id(pool: MySqlPool) {
    let (server, stubs) = setup(&pool).await;
    for key in [None, Some("wrong-key")] {
        let mut request = server.post("/v1/emails/inbound-push");
        if let Some(key) = key {
            request = request.add_header("authorization", key);
        }
        request
            .json(&json!({ "email_id": 8802 }))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    for body in [json!({}), json!({ "email_id": "8802x" })] {
        server
            .post("/v1/emails/inbound-push")
            .add_header("authorization", JOB_KEY)
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    assert!(email_pushes(&stubs).is_empty());
}
