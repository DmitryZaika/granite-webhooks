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
