use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::Value;
use sqlx::MySqlPool;

// Seed: company 100. Rita (100) is its sales rep, Mark (101) its sales
// manager. Leads here are made now, so they fall in the 14-day window.

async fn seed_leads(pool: &MySqlPool) {
    sqlx::query(
        "INSERT INTO customers (id, name, phone, company_id, source, sales_rep, created_date, assigned_date, invalid_lead) VALUES
          (9100, 'New Unassigned', '317-555-9100', 100, 'leads',    NULL, NOW() - INTERVAL 1 HOUR, NULL, NULL),
          (9101, 'Rita New',       '317-555-9101', 100, 'leads',    100,  NOW() - INTERVAL 2 HOUR, NOW(), NULL),
          (9102, 'Mark Own',       NULL,           100, 'leads',    101,  NOW() - INTERVAL 3 HOUR, NOW() - INTERVAL 1 HOUR, NULL),
          (9103, 'Old Unassigned', NULL,           100, 'leads',    NULL, NOW() - INTERVAL 30 DAY, NULL, NULL),
          (9104, 'Spam Lead',      NULL,           100, 'leads',    NULL, NOW(), NULL, 'spam'),
          (9105, 'Walk In',        NULL,           100, 'check-in', NULL, NOW(), NULL, NULL),
          (9106, 'Rita Old',       NULL,           100, 'leads',    100,  NOW() - INTERVAL 40 DAY, NOW() - INTERVAL 20 DAY, NULL)",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn ids(list: &Value) -> Vec<i64> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|lead| lead["customer_id"].as_i64().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn a_rep_sees_only_their_recent_leads(pool: MySqlPool) {
    seed_leads(&pool).await;
    let res = server(pool)
        .get("/v1/customers/lead-inbox")
        .with_session(sessions::REP)
        .await;
    res.assert_status(StatusCode::OK);
    let inbox: Value = res.json();
    assert_eq!(inbox["is_manager"], false);
    assert_eq!(ids(&inbox["assigned"]), [9101]);
    assert_eq!(inbox["assigned"][0]["sales_rep_name"], "Rita Rep");
    assert_eq!(ids(&inbox["unassigned"]), Vec::<i64>::new());
    assert_eq!(inbox["reps"], serde_json::json!([]));
}

#[test_db::test(seed)]
async fn a_manager_sees_unassigned_leads_and_the_reps(pool: MySqlPool) {
    seed_leads(&pool).await;
    let res = server(pool)
        .get("/v1/customers/lead-inbox")
        .with_session(sessions::MANAGER)
        .await;
    res.assert_status(StatusCode::OK);
    let inbox: Value = res.json();
    assert_eq!(inbox["is_manager"], true);
    assert_eq!(ids(&inbox["unassigned"]), [9100]);
    assert_eq!(inbox["unassigned"][0]["deal_id"], Value::Null);
    assert_eq!(ids(&inbox["assigned"]), [9101, 9102]);
    let reps = inbox["reps"].as_array().unwrap();
    assert_eq!(reps.len(), 1);
    assert_eq!(reps[0]["id"], 100);
    assert!(reps[0]["month_leads"].as_i64().unwrap() >= 1);
}
