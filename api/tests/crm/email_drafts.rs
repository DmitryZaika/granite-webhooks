use crate::common;

use api::routes::emails::attachment_token;
use axum::http::StatusCode;
use common::{SECRET, WithSession, server, sessions};
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
                "content_type": "application/pdf",
                "token": attachment_token::sign(SECRET, "https://files.test/bucket/emails/a.pdf", 107),
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

/// A draft takes only files the caller may send: uploaded by them (the
/// upload's token) or already on the company's mail. A saved draft's files
/// are trusted when it is sent or previewed, so any other URL (another
/// company's file, the private signatures bucket) is refused.
#[test_db::test(seed)]
async fn drafts_take_only_files_the_caller_may_send(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO emails (id, thread_id, subject, body, sender_email, receiver_email, sent_at, company_id, message_id) VALUES
           (9801, 'other-files', 'Plans', 'x', 'buyer@other-customer.test', 'rep@other.test', NOW(), 101, '<f1@x>');
         INSERT INTO email_attachments (id, email_id, content_type, content_subtype, filename, url) VALUES
           (9811, 9801, 'application', 'pdf', 'plans.pdf', 'https://files.test/bucket/emails/101/plans.pdf');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let draft = |url: &str, token: Option<String>| {
        json!({
            "subject": "Files",
            "attachments": [{ "url": url, "filename": "f.pdf", "content_type": "application/pdf", "token": token }],
        })
    };
    let own = "https://files.test/bucket/emails/100/mine.pdf";
    for (url, token) in [
        ("https://files.test/bucket/emails/101/plans.pdf", None),
        (
            "https://files.test/bucket/emails/101/plans.pdf",
            Some(attachment_token::sign(
                SECRET,
                "https://files.test/bucket/emails/101/plans.pdf",
                106,
            )),
        ),
        ("s3://granite-signatures/signatures/9/completed-x.pdf", None),
        (own, None),
        (own, Some("00".repeat(32))),
        // Another user's token.
        (own, Some(attachment_token::sign(SECRET, own, 100))),
    ] {
        let res = server
            .post("/v1/emails/drafts")
            .with_session(DINA)
            .json(&draft(url, token))
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], "You cannot attach f.pdf");
    }

    let res = server
        .post("/v1/emails/drafts")
        .with_session(DINA)
        .json(&draft(own, Some(attachment_token::sign(SECRET, own, 107))))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    // Reopened and saved again, without a token: the file is on the draft.
    server
        .put(&format!("/v1/emails/drafts/{id}"))
        .with_session(DINA)
        .json(&draft(own, None))
        .await
        .assert_status_ok();
    let reopened: Value = server
        .get(&format!("/v1/emails/drafts/{id}"))
        .with_session(DINA)
        .await
        .json();
    assert_eq!(
        reopened["attachments"],
        json!([{ "url": own, "filename": "f.pdf", "content_type": "application/pdf" }])
    );
    // Nothing planted on the other company's file either.
    server
        .put(&format!("/v1/emails/drafts/{id}"))
        .with_session(DINA)
        .json(&draft(
            "https://files.test/bucket/emails/101/plans.pdf",
            None,
        ))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

/// A file of the company's mail (forwarding an attachment) needs no token;
/// another company's never does, and a token proves only its own URL.
#[test_db::test(seed)]
async fn draft_files_follow_the_send_rules(pool: MySqlPool) {
    let server = server(pool.clone());
    let draft = |url: &str, token: Option<String>| {
        json!({
            "subject": "Files",
            "attachments": [{ "url": url, "filename": "f.png", "content_type": "image/png", "token": token }],
        })
    };
    // Seeded attachment 8810 is on Olga's company's thread.
    let sketch = "https://example.test/sketch.png";
    server
        .post("/v1/emails/drafts")
        .with_session(OLGA)
        .json(&draft(sketch, None))
        .await
        .assert_status(StatusCode::CREATED);
    server
        .post("/v1/emails/drafts")
        .with_session(DINA)
        .json(&draft(sketch, None))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    // Dina's token for one upload does not cover another URL.
    let mine = "https://files.test/bucket/emails/100/a.png";
    server
        .post("/v1/emails/drafts")
        .with_session(DINA)
        .json(&draft(
            sketch,
            Some(attachment_token::sign(SECRET, mine, 107)),
        ))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

/// One file the caller may not attach refuses the whole save: no draft is
/// created, and an existing draft keeps its content and files.
#[test_db::test(seed)]
async fn a_refused_file_saves_nothing(pool: MySqlPool) {
    let server = server(pool.clone());
    let good = "https://files.test/bucket/emails/100/good.pdf";
    let token = attachment_token::sign(SECRET, good, 107);
    let files = |bad: bool| {
        let mut list = vec![
            json!({ "url": good, "filename": "good.pdf", "content_type": "application/pdf", "token": token }),
        ];
        if bad {
            list.push(json!({ "url": "https://example.test/sketch.png", "filename": "sketch.png", "content_type": "image/png" }));
        }
        list
    };
    let drafts = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM emails WHERE sender_user_id = 107 AND is_draft = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let before = drafts().await;
    server
        .post("/v1/emails/drafts")
        .with_session(DINA)
        .json(&json!({ "subject": "Both", "attachments": files(true) }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(drafts().await, before);

    let res = server
        .post("/v1/emails/drafts")
        .with_session(DINA)
        .json(&json!({ "subject": "Good", "attachments": files(false) }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    server
        .put(&format!("/v1/emails/drafts/{id}"))
        .with_session(DINA)
        .json(&json!({ "subject": "Changed", "attachments": files(true) }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let draft: Value = server
        .get(&format!("/v1/emails/drafts/{id}"))
        .with_session(DINA)
        .await
        .json();
    assert_eq!(draft["subject"], "Good");
    assert_eq!(
        draft["attachments"],
        json!([{ "url": good, "filename": "good.pdf", "content_type": "application/pdf" }])
    );
}
