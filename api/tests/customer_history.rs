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
