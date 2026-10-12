mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: "Mailbox" block (emails 8800-8807, threads aaaaaaaa-1111-...-00000000000N).

const T1: &str = "aaaaaaaa-1111-4000-8000-000000000001";
const T2: &str = "aaaaaaaa-1111-4000-8000-000000000002";
const T3: &str = "aaaaaaaa-1111-4000-8000-000000000003";
const T4: &str = "aaaaaaaa-1111-4000-8000-000000000004";
const T5: &str = "aaaaaaaa-1111-4000-8000-000000000005";
const T6: &str = "aaaaaaaa-1111-4000-8000-000000000006";
const OLGA: &str = sessions::OTHER_COMPANY_REP;

async fn get(pool: &MySqlPool, path: &str, session: &str) -> Value {
    let res = server(pool.clone()).get(path).with_session(session).await;
    res.assert_status_ok();
    res.json()
}

fn email_ids(page: &Value) -> Vec<i64> {
    page["emails"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_i64().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn inbox_lists_threads_addressed_to_the_caller(pool: MySqlPool) {
    let page = get(&pool, "/v1/emails/threads", OLGA).await;
    // T1 (latest mail to Olga 04-11) then T2; every message of T1, newest first.
    assert_eq!(email_ids(&page), vec![8802, 8801, 8800, 8803]);
    assert_eq!(page["total_count"], 2);
    assert_eq!(page["page"], 1);
    assert_eq!(page["page_size"], 25);
    assert_eq!(
        page["emails"][0],
        json!({
            "id": 8802,
            "thread_id": T1,
            "subject": "Re: Kitchen quote",
            "body": "Thanks! See my sketch.",
            "sent_at": "2026-04-11T09:00:00.000Z",
            "sender_email": "buyer@other-customer.test",
            "receiver_email": "rep@other.test",
            "sender_user_id": null,
            "employee_read_at": null,
            "is_draft": false,
            "client_read_at": null,
            "has_attachments": true,
            "thread_has_attachments": true,
            "sender_name": "Other Co Customer",
            "receiver_name": "Olga Other",
            "inbox_sort_at": "2026-04-11T09:00:00.000Z"
        })
    );
    // Olga's reply: the open counts, the name comes from the users table.
    assert_eq!(
        page["emails"][1]["client_read_at"],
        "2026-04-10T15:00:00.000Z"
    );
    assert_eq!(page["emails"][1]["sender_name"], "Olga Other");
    // Pat has no customer row: the name from the participant header.
    assert_eq!(page["emails"][3]["sender_name"], "Pat Prospect");
}

#[test_db::test(seed)]
async fn sent_trash_and_drafts(pool: MySqlPool) {
    let sent = get(&pool, "/v1/emails/threads?folder=sent", OLGA).await;
    assert_eq!(email_ids(&sent), vec![8801, 8804]);
    // Opened a second after sending: an image proxy, not the customer.
    assert_eq!(sent["emails"][1]["client_read_at"], Value::Null);
    let trash = get(&pool, "/v1/emails/threads?folder=trash", OLGA).await;
    assert_eq!(email_ids(&trash), vec![8805]);
    let drafts = get(&pool, "/v1/emails/threads?folder=drafts", OLGA).await;
    assert_eq!(email_ids(&drafts), vec![8806]);
    assert_eq!(drafts["emails"][0]["is_draft"], true);
    assert_eq!(
        get(&pool, "/v1/emails/folder-counts", OLGA).await,
        json!({ "inbox_unread": 1, "trash": 1, "drafts": 1 })
    );
}

#[test_db::test(seed)]
async fn search_matches_text_and_customer_names(pool: MySqlPool) {
    // In the inbox a hit anywhere in the thread lists the whole thread.
    let page = get(&pool, "/v1/emails/threads?search=sketch.", OLGA).await;
    assert_eq!(email_ids(&page), vec![8802, 8801, 8800]);
    let page = get(&pool, "/v1/emails/threads?search=black%20pearl", OLGA).await;
    assert_eq!(email_ids(&page), vec![8803]);
    // A customer name matches that customer's addresses.
    let page = get(
        &pool,
        "/v1/emails/threads?folder=sent&search=Other%20Co",
        OLGA,
    )
    .await;
    assert_eq!(email_ids(&page), vec![8801, 8804]);
    let page = get(&pool, "/v1/emails/threads?search=nothing-like-this", OLGA).await;
    assert_eq!(page["total_count"], 0);
}

#[test_db::test(seed)]
async fn mailboxes_are_per_user_and_company(pool: MySqlPool) {
    let rita = get(&pool, "/v1/emails/threads", sessions::REP).await;
    assert_eq!(email_ids(&rita), vec![8807]);
    let app = server(pool.clone());
    app.get("/v1/emails/threads?folder=junk")
        .with_session(OLGA)
        .await
        .assert_status_bad_request();
    app.get("/v1/emails/threads")
        .await
        .assert_status_unauthorized();
}

#[test_db::test(seed)]
async fn unread_badge_needs_a_mail_domain(pool: MySqlPool) {
    assert_eq!(
        get(&pool, "/v1/emails/unread", OLGA).await,
        json!({ "count": 1, "latest_inbound_id": 8802 })
    );
    // Company 100 has no domain.
    assert_eq!(
        get(&pool, "/v1/emails/unread", sessions::REP).await,
        json!({ "count": 0, "latest_inbound_id": null })
    );
}

#[test_db::test(seed)]
async fn a_thread_with_its_customer_messages_and_participants(pool: MySqlPool) {
    let thread = get(&pool, &format!("/v1/emails/threads/{T1}"), OLGA).await;
    assert_eq!(thread["deal_id"], 8009);
    assert_eq!(
        thread["customer"],
        json!({ "id": 2000, "name": "Other Co Customer", "email": "buyer@other-customer.test" })
    );
    assert_eq!(thread["subject"], "Kitchen quote");
    assert_eq!(thread["sending_email"], "rep@other.test");
    let ids: Vec<i64> = thread["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![8800, 8801, 8802]);
    assert_eq!(thread["messages"][1]["html_body"], "<p>Sure, attached.</p>");
    assert_eq!(thread["messages"][1]["read_at"], "2026-04-10T15:00:00.000Z");
    assert_eq!(
        thread["messages"][2]["attachments"],
        json!([{ "id": 8810, "email_id": 8802, "content_type": "image", "content_subtype": "png",
                 "filename": "sketch.png", "url": "https://example.test/sketch.png",
                 "signed_url": null, "signature_status": null }])
    );
    // BCC never leaves the server.
    let participants = thread["participants"].as_array().unwrap();
    assert_eq!(participants.len(), 5);
    assert!(participants.iter().all(|p| p["type"] != "bcc"));
    assert_eq!(
        participants[4],
        json!({ "email_id": 8801, "type": "cc", "email": "boss@other-customer.test",
                "display_name": "Boss", "user_id": null, "customer_id": null,
                "user_name": null, "customer_name": null })
    );
    // Reading does not mark read.
    let unread: Option<String> =
        sqlx::query_scalar("SELECT CAST(employee_read_at AS CHAR) FROM emails WHERE id = 8802")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(unread, None);
}

#[test_db::test(seed)]
async fn thread_customers_without_a_deal(pool: MySqlPool) {
    // No customer row: the header name; no header name either: the address.
    let thread = get(&pool, &format!("/v1/emails/threads/{T2}"), OLGA).await;
    assert_eq!(
        thread["customer"],
        json!({ "id": null, "name": "Pat Prospect", "email": "pat@prospect.test" })
    );
    let thread = get(&pool, &format!("/v1/emails/threads/{T3}"), OLGA).await;
    assert_eq!(thread["customer"]["id"], 2000);
    // Trash shows the deleted messages only.
    let trash = get(
        &pool,
        &format!("/v1/emails/threads/{T4}?folder=trash"),
        OLGA,
    )
    .await;
    assert_eq!(trash["messages"].as_array().unwrap().len(), 1);
    let live = get(&pool, &format!("/v1/emails/threads/{T4}"), OLGA).await;
    assert_eq!(live["messages"], json!([]));
    // Another company's thread.
    server(pool)
        .get(&format!("/v1/emails/threads/{T6}"))
        .with_session(OLGA)
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn opening_and_bulk_read_state(pool: MySqlPool) {
    let app = server(pool.clone());
    app.post(&format!("/v1/emails/threads/{T1}/read"))
        .with_session(OLGA)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let rows: Vec<(i32, Option<String>)> = sqlx::query_as(
        "SELECT id, CAST(employee_read_at AS CHAR) FROM emails WHERE thread_id = ? ORDER BY id",
    )
    .bind(T1)
    .fetch_all(&pool)
    .await
    .unwrap();
    // Her own reply stays unstamped.
    assert!(rows[2].1.is_some());
    assert_eq!(rows[1].1, None);
    assert_eq!(
        get(&pool, "/v1/emails/folder-counts", OLGA).await["inbox_unread"],
        0
    );

    app.post("/v1/emails/threads/mark-unread")
        .with_session(OLGA)
        .json(&json!({ "thread_ids": [T1] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        get(&pool, "/v1/emails/folder-counts", OLGA).await["inbox_unread"],
        1
    );
    app.post("/v1/emails/threads/mark-read")
        .with_session(OLGA)
        .json(&json!({ "thread_ids": [T1] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        get(&pool, "/v1/emails/folder-counts", OLGA).await["inbox_unread"],
        0
    );

    // Another company's thread is left alone; another company's thread cannot be opened.
    app.post("/v1/emails/threads/mark-read")
        .with_session(OLGA)
        .json(&json!({ "thread_ids": [T6] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let acme: Option<String> =
        sqlx::query_scalar("SELECT CAST(employee_read_at AS CHAR) FROM emails WHERE id = 8807")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(acme, None);
    app.post(&format!("/v1/emails/threads/{T6}/read"))
        .with_session(OLGA)
        .await
        .assert_status_not_found();
    app.post("/v1/emails/threads/mark-read")
        .with_session(OLGA)
        .json(&json!({ "thread_ids": [] }))
        .await
        .assert_status_bad_request();
}

#[test_db::test(seed)]
async fn trash_restore_and_purge(pool: MySqlPool) {
    let app = server(pool.clone());
    app.post("/v1/emails/threads/delete")
        .with_session(OLGA)
        .json(&json!({ "thread_ids": [T2, T6] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let trash = get(&pool, "/v1/emails/threads?folder=trash", OLGA).await;
    assert_eq!(email_ids(&trash), vec![8803, 8805]);
    assert_eq!(
        get(&pool, "/v1/emails/threads", sessions::REP).await["total_count"],
        1,
        "Acme's untouched"
    );

    app.post("/v1/emails/threads/restore")
        .with_session(OLGA)
        .json(&json!({ "thread_ids": [T2] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let res = app
        .post("/v1/emails/threads/purge")
        .with_session(OLGA)
        .json(&json!({ "thread_ids": [T4, T1] }))
        .await;
    res.assert_status_ok();
    // Only trashed messages go; T1 is live.
    assert_eq!(res.json::<Value>(), json!({ "purged": 1 }));
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM emails WHERE thread_id IN (?, ?)")
        .bind(T4)
        .bind(T1)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 3);
    let too_many: Vec<String> = (0..51).map(|i| format!("t{i}")).collect();
    app.post("/v1/emails/threads/delete")
        .with_session(OLGA)
        .json(&json!({ "thread_ids": too_many }))
        .await
        .assert_status_bad_request();
    let _ = T5;
}

#[test_db::test(seed)]
async fn search_suggestions_per_conversation(pool: MySqlPool) {
    let rows = get(&pool, "/v1/emails/search-suggestions?term=kitchen", OLGA).await;
    // The newest matching message of T1 only.
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["thread_id"], T1);
    assert_eq!(rows[0]["sender_name"], "Other Co Customer");
    assert_eq!(
        get(&pool, "/v1/emails/search-suggestions?term=k", OLGA).await,
        json!([])
    );
    let sent = get(
        &pool,
        "/v1/emails/search-suggestions?term=following&folder=sent",
        OLGA,
    )
    .await;
    assert_eq!(sent[0]["thread_id"], T3);
    // The admin scope needs an admin; it sees the whole company's mail.
    server(pool.clone())
        .get("/v1/emails/search-suggestions?term=kitchen&scope=admin")
        .with_session(OLGA)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let acme = get(
        &pool,
        "/v1/emails/search-suggestions?term=acme&scope=admin",
        sessions::ADMIN,
    )
    .await;
    assert_eq!(acme[0]["thread_id"], T6);
}

#[test_db::test(seed)]
async fn deal_email_history(pool: MySqlPool) {
    let history = get(&pool, "/v1/deals/8009/emails", OLGA).await;
    let ids = |v: &Value| -> Vec<i64> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_i64().unwrap())
            .collect()
    };
    // T1 is on the deal; the customer has one deal, so mail to her address
    // too (T3, and the draft T5: the address query never filtered drafts).
    assert_eq!(ids(&history["emails"]), vec![8802, 8801, 8800, 8806, 8804]);
    assert_eq!(ids(&history["thread_emails"]), vec![8802, 8801, 8800]);
    assert_eq!(history["customer_emails"], json!([]));
    assert_eq!(
        history["emails"][1],
        json!({
            "id": 8801, "deal_id": 8009, "thread_id": T1, "subject": "Re: Kitchen quote",
            "body": "Sure, attached.", "sent_at": "2026-04-10T11:00:00.000Z", "read_count": 1,
            "has_attachments": false, "sender_user_id": 106,
            "sender_email": "Olga Other <rep@other.test>", "sender_name": "Olga Other",
            "employee_read_at": null, "thread_has_attachments": true
        })
    );
    server(pool)
        .get("/v1/deals/8009/emails")
        .with_session(sessions::REP)
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn sent_drips_missing_from_history_are_recorded(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO email_templates (id, template_name, template_subject, template_body, company_id, show_template)
           VALUES (8890, 'Thanks', 'Thank you!', 'Hi {{customer.first_name}}, from {{user.first_name}}', 101, 1);
         INSERT INTO scheduled_emails (id, template_id, deal_id, list_id, customer_id, user_id, company_id, send_at, status, sent_at, message_id)
           VALUES (8891, 8890, 8009, 103, 2000, 106, 101, '2026-04-12 10:00:00', 'sent', '2026-04-12 10:00:00', '<ses-8891@amazonses.com>');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let history = get(&pool, "/v1/deals/8009/emails", OLGA).await;
    let first = &history["emails"][0];
    assert_eq!(first["subject"], "Thank you!");
    assert_eq!(first["body"], "Hi Other, from Olga");
    assert_eq!(first["sent_at"], "2026-04-12T10:00:00.000Z");
    // To the primary address of the customer. Only once.
    let again = get(&pool, "/v1/deals/8009/emails", OLGA).await;
    assert_eq!(
        again["emails"].as_array().unwrap().len(),
        history["emails"].as_array().unwrap().len()
    );
    let participants: Vec<(String, String)> = sqlx::query_as(
        "SELECT CAST(ep.type AS CHAR), ep.email FROM email_participants ep JOIN emails e ON e.id = ep.email_id
         WHERE e.message_id = 'ses-8891@amazonses.com' ORDER BY ep.id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        participants,
        vec![
            ("from".to_string(), "rep@other.test".to_string()),
            ("to".to_string(), "other@otherco.example".to_string())
        ]
    );
}
