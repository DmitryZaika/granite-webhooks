mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::Value;
use sqlx::MySqlPool;

// Seed: Nina Lead (8695, company 100) has live deals 8600 ("New") and 8607
// ("Measure"), the deleted deal 8605, the notes 8630 (pinned, comments
// "Agreed" and "Anonymous") and 8631 on 8600, activities 8620/8621 on 8600.
// Olga (company 101) may not see her.

async fn history(pool: &MySqlPool, session: &str, id: i32) -> axum_test::TestResponse {
    server(pool.clone())
        .get(&format!("/v1/customers/{id}/history"))
        .with_session(session)
        .await
}

fn ids(list: &Value) -> Vec<i64> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_i64().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn the_customer_history_has_deals_notes_and_activities(pool: MySqlPool) {
    let res = history(&pool, sessions::REP, 8695).await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(ids(&body["deals"]), vec![8607, 8600]);
    assert_eq!(body["deals"][1]["list_name"], "New");
    assert_eq!(body["deals"][1]["amount"], "1500.25");
    let created = ids(&body["created_deals"]);
    assert!(created.contains(&8605) && created.contains(&8600));
    assert_eq!(ids(&body["deleted_deals"]), vec![8605]);
    assert_eq!(body["deleted_at"], Value::Null);

    assert_eq!(ids(&body["notes"]), vec![8630, 8631]);
    assert_eq!(body["notes"][0]["is_pinned"], 1);
    let comments: Vec<&str> = body["notes"][0]["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["content"].as_str().unwrap())
        .collect();
    assert_eq!(comments, ["Agreed", "Anonymous"]);

    let activities = ids(&body["activities"]);
    assert!(activities.contains(&8620) && activities.contains(&8621));
    let call_back = body["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == 8620)
        .unwrap();
    assert_eq!(call_back["deadline"], "2026-01-05T09:30:00");
    assert_eq!(call_back["priority"], "high");
    assert_eq!(call_back["deal_list_name"], "New");

    assert!(body["first_rep_deal_created_at"].is_string());
    assert_eq!(body["initial_sales_rep_name"], "Dina Dealer");
}

#[test_db::test(seed)]
async fn other_companies_customers_are_not_found(pool: MySqlPool) {
    history(&pool, sessions::OTHER_COMPANY_REP, 8695)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    history(&pool, sessions::REP, 999_999)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server(pool)
        .get("/v1/customers/8695/history")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn the_mailbox_customer_has_email_history(pool: MySqlPool) {
    let body: Value = history(&pool, sessions::OTHER_COMPANY_REP, 2000)
        .await
        .json();
    let subjects: Vec<&str> = body["emails"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["subject"].as_str())
        .collect();
    assert!(subjects.contains(&"Re: Kitchen quote"), "{subjects:?}");
}

#[test_db::test(seed)]
async fn another_customers_mail_in_a_shared_thread_is_not_shown(pool: MySqlPool) {
    // One thread holds a message to Nina (8695) and one to somebody else.
    sqlx::query(
        "INSERT INTO emails (id, subject, body, sent_at, message_id, thread_id, sender_user_id,
           sender_email, receiver_email, company_id, is_draft) VALUES
         (98001, 'Invoice for Lindy', 'b', '2026-04-02 10:00:00', '<m98001@acme.test>',
          'bbbbbbbb-2222-4000-8000-000000000001', 100, 'rep@acme.test', 'lindy@elsewhere.test', 100, 0),
         (98002, 'Quote for Nina', 'b', '2026-04-02 11:00:00', '<m98002@acme.test>',
          'bbbbbbbb-2222-4000-8000-000000000001', 100, 'rep@acme.test', 'nina@example.com', 100, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO email_participants (email_id, type, email, customer_id, position) VALUES
         (98001, 'to', 'lindy@elsewhere.test', NULL, 0),
         (98002, 'to', 'nina@example.com', 8695, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let body: Value = history(&pool, sessions::REP, 8695).await.json();
    let emails = ids(&body["emails"]);
    assert!(emails.contains(&98002), "{emails:?}");
    assert!(!emails.contains(&98001), "{emails:?}");
}

#[test_db::test(seed)]
async fn another_companys_mail_is_not_shown_for_its_address(pool: MySqlPool) {
    // A rep of company 100 saves company 101's inbox address on a customer.
    let server = server(pool.clone());
    let created = server
        .post("/v1/customers")
        .with_session(sessions::REP)
        .json(&serde_json::json!({
            "name": "Planted", "emails": ["rep@other.test"], "source": "other"
        }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let id = created.json::<Value>()["id"].as_i64().unwrap();
    // Unstamped inbound mail to that address is not the rep's either.
    sqlx::query(
        "INSERT INTO emails (id, subject, body, sent_at, message_id, thread_id,
           sender_email, receiver_email, company_id, is_draft) VALUES
         (98101, 'Unstamped inbound', 'b', '2026-04-03 10:00:00', '<m98101@x.test>',
          'bbbbbbbb-3333-4000-8000-000000000001', 'buyer@x.test', 'rep@other.test', NULL, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let body: Value = server
        .get(&format!("/v1/customers/{id}/history"))
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        ids(&body["emails"]),
        Vec::<i64>::new(),
        "{}",
        body["emails"]
    );
}

#[test_db::test(seed)]
async fn another_companys_stamped_mail_is_not_shown_for_its_customers_address(pool: MySqlPool) {
    // A rep of company 100 saves the addresses of company 101's customer and
    // prospect on a customer of their own. 101's mail with them (8800-8804) is
    // stamped 101; the inbound part has no sending user.
    let server = server(pool.clone());
    let created = server
        .post("/v1/customers")
        .with_session(sessions::REP)
        .json(&serde_json::json!({
            "name": "Planted",
            "emails": ["buyer@other-customer.test", "pat@prospect.test"],
            "source": "other"
        }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let id = created.json::<Value>()["id"].as_i64().unwrap();

    let res = server
        .get(&format!("/v1/customers/{id}/history"))
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(
        ids(&body["emails"]),
        Vec::<i64>::new(),
        "{}",
        body["emails"]
    );

    // Company 101 still has all of it.
    let theirs = ids(&history(&pool, sessions::OTHER_COMPANY_REP, 2000)
        .await
        .json::<Value>()["emails"]);
    for id in [8800, 8801, 8802] {
        assert!(theirs.contains(&id), "{theirs:?}");
    }
}

/// Mail with Nina's addresses (company 100's customer 8695) from both
/// companies, stamped, legacy and unstamped. Olga's customer 2000 is given
/// Nina's address too, as if she had planted it.
async fn mail_with_ninas_addresses(pool: &MySqlPool) {
    sqlx::query(
        "INSERT INTO emails (id, subject, body, sent_at, message_id, thread_id, sender_user_id,
           sender_email, receiver_email, company_id, is_draft) VALUES
         (98201, 'Inbound to 100', 'b', '2026-04-03 10:00:00', '<m98201@x.test>',
          'bbbbbbbb-4444-4000-8000-000000000001', NULL, 'Nina <nina@example.com>', 'rep@acme.test', 100, 0),
         (98202, 'Legacy from 100', 'b', '2026-04-03 11:00:00', '<m98202@x.test>',
          'bbbbbbbb-4444-4000-8000-000000000002', 100, 'rep@acme.test', 'NINA.WORK@example.com', NULL, 0),
         (98203, 'Legacy from 101', 'b', '2026-04-03 12:00:00', '<m98203@x.test>',
          'bbbbbbbb-4444-4000-8000-000000000003', 106, 'rep@other.test', 'nina@example.com', NULL, 0),
         (98204, 'Inbound to 101', 'b', '2026-04-03 13:00:00', '<m98204@x.test>',
          'bbbbbbbb-4444-4000-8000-000000000004', NULL, 'nina@example.com', 'rep@other.test', 101, 0),
         (98205, '101 with Nina as participant', 'b', '2026-04-03 14:00:00', '<m98205@x.test>',
          'bbbbbbbb-4444-4000-8000-000000000005', NULL, 'someone@x.test', 'rep@other.test', 101, 0),
         (98206, '100 with Nina as participant', 'b', '2026-04-03 15:00:00', '<m98206@x.test>',
          'bbbbbbbb-4444-4000-8000-000000000006', NULL, 'someone@x.test', 'rep@acme.test', 100, 0),
         (98207, 'Unstamped inbound', 'b', '2026-04-03 16:00:00', '<m98207@x.test>',
          'bbbbbbbb-4444-4000-8000-000000000007', NULL, 'nina@example.com', 'nobody@x.test', NULL, 0)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO email_participants (email_id, type, email, customer_id, position) VALUES
         (98205, 'cc', 'nina@example.com', 8695, 0),
         (98206, 'cc', 'nina@example.com', 8695, 0)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO customers_emails (customer_id, email, created_at, created_by)
         VALUES (2000, 'nina@example.com', '2026-04-01 08:00:00', 106)",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn ninas_mail(body: &Value) -> Vec<i64> {
    let mut found: Vec<i64> = ids(&body["emails"])
        .into_iter()
        .filter(|id| (98201..=98207).contains(id))
        .collect();
    found.sort_unstable();
    found
}

#[test_db::test(seed)]
async fn each_company_sees_only_its_own_mail_with_a_shared_address(pool: MySqlPool) {
    mail_with_ninas_addresses(&pool).await;

    // 100: its stamped mail, its legacy mail (by the sender's company), and
    // the mail Nina is a participant of, but only when it is 100's.
    let body: Value = history(&pool, sessions::REP, 8695).await.json();
    assert_eq!(ninas_mail(&body), vec![98201, 98202, 98206]);

    // 101 has the same address on its customer: it sees its own mail only.
    let body: Value = history(&pool, sessions::OTHER_COMPANY_REP, 2000)
        .await
        .json();
    assert_eq!(ninas_mail(&body), vec![98203, 98204]);
}
