mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: Olga (106, company 101) has draft 8806 "Draft quote" (T5) to
// buyer@other-customer.test, without participant rows. Dina (107, company
// 100) owns deal 8600.

const OLGA: &str = sessions::OTHER_COMPANY_REP;
const DINA: &str = sessions::DEALER;

#[test_db::test(seed)]
async fn a_seeded_draft_reopens_with_its_receiver(pool: MySqlPool) {
    let res = server(pool)
        .get("/v1/emails/drafts/8806")
        .with_session(OLGA)
        .await;
    res.assert_status_ok();
    let draft: Value = res.json();
    assert_eq!(
        draft,
        json!({
            "id": 8806,
            "thread_id": "aaaaaaaa-1111-4000-8000-000000000005",
            "subject": "Draft quote",
            "body": "Work in progress",
            "deal_id": null,
            "to": ["buyer@other-customer.test"],
            "cc": [],
            "bcc": [],
            "send_together": false,
            "attachments": [],
        })
    );
}

#[test_db::test(seed)]
async fn drafts_are_saved_reopened_updated_and_deleted(pool: MySqlPool) {
    let server = server(pool.clone());
    let res = server
        .post("/v1/emails/drafts")
        .with_session(DINA)
        .json(&json!({
            "subject": " Quote ",
            "body": "<p>Almost ready</p>",
            "to": ["Nina@example.com", "rep@acme.test"],
            "cc": ["nina@example.com", "boss@x.test"],
            "deal_id": 8600,
            "send_together": true,
            "attachments": [{
                "url": "https://files.test/bucket/emails/a.pdf",
                "filename": "a.pdf",
                "content_type": "application/pdf"
            }],
        }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let saved: Value = res.json();
    let id = saved["id"].as_i64().unwrap();
    let thread = saved["thread_id"].as_str().unwrap().to_string();

    let draft: Value = server
        .get(&format!("/v1/emails/drafts/{id}"))
        .with_session(DINA)
        .await
        .json();
    assert_eq!(draft["subject"], "Quote");
    assert_eq!(draft["body"], "<p>Almost ready</p>");
    assert_eq!(draft["to"], json!(["nina@example.com", "rep@acme.test"]));
    assert_eq!(draft["cc"], json!(["boss@x.test"]));
    assert_eq!(draft["deal_id"], 8600);
    assert_eq!(draft["send_together"], true);
    assert_eq!(
        draft["attachments"],
        json!([{ "url": "https://files.test/bucket/emails/a.pdf", "filename": "a.pdf", "content_type": "application/pdf" }])
    );
    // Recipients are linked like a sent message's.
    let linked: Vec<(String, Option<i32>, Option<i32>)> = sqlx::query_as(
        "SELECT email, user_id, customer_id FROM email_participants WHERE email_id = ? AND type = 'to' ORDER BY position",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        linked,
        [
            ("nina@example.com".into(), None, Some(8695)),
            ("rep@acme.test".into(), Some(100), None)
        ]
    );

    let res = server
        .put(&format!("/v1/emails/drafts/{id}"))
        .with_session(DINA)
        .json(&json!({ "subject": "Quote v2", "body": "", "to": ["x@y.test"] }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["thread_id"], thread);
    let draft: Value = server
        .get(&format!("/v1/emails/drafts/{id}"))
        .with_session(DINA)
        .await
        .json();
    assert_eq!(draft["subject"], "Quote v2");
    assert_eq!(draft["to"], json!(["x@y.test"]));
    assert_eq!(draft["cc"], json!([]));
    assert_eq!(draft["send_together"], false);
    assert_eq!(draft["deal_id"], Value::Null);
    assert_eq!(draft["attachments"], json!([]));

    server
        .delete(&format!("/v1/emails/drafts/{id}"))
        .with_session(DINA)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get(&format!("/v1/emails/drafts/{id}"))
        .with_session(DINA)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn only_the_author_sees_or_changes_a_draft(pool: MySqlPool) {
    let server = server(pool);
    for session in [DINA, sessions::ADMIN] {
        server
            .get("/v1/emails/drafts/8806")
            .with_session(session)
            .await
            .assert_status(StatusCode::NOT_FOUND);
        server
            .put("/v1/emails/drafts/8806")
            .with_session(session)
            .json(&json!({ "subject": "Mine now" }))
            .await
            .assert_status(StatusCode::NOT_FOUND);
        server
            .delete("/v1/emails/drafts/8806")
            .with_session(session)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    // A sent message is not a draft.
    server
        .get("/v1/emails/drafts/8801")
        .with_session(OLGA)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn empty_drafts_and_foreign_deals_are_refused(pool: MySqlPool) {
    let server = server(pool);
    let res = server
        .post("/v1/emails/drafts")
        .with_session(DINA)
        .json(&json!({ "subject": " ", "body": "<p><br></p>", "to": ["a@b.test"] }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "Draft is empty");
    let res = server
        .post("/v1/emails/drafts")
        .with_session(OLGA)
        .json(&json!({ "subject": "Hi", "deal_id": 8600 }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "Deal not found");
}
