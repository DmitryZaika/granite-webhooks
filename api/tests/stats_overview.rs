mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: Dina's (107) April 2026 deals 8600-8607: open 8600 ($1,500.25),
// 8601 (no amount), 8602 ($300), 8607 ($30, Remodel); won 8603 ($999); lost
// 8604 ($50, "Too pricey"); 8605 deleted; 8606 of a deleted customer.

#[test_db::test(seed)]
async fn the_statistics_page_counts_a_reps_month(pool: MySqlPool) {
    let server = server(pool);
    let url = "/v1/stats/overview?from_date=2026-04-30&to_date=2026-04-01&sales_rep=Dina%20Dealer";
    server
        .get(url)
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let body = server
        .get(url)
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    // The days come back in order.
    assert_eq!(body["from_date"], "2026-04-01");
    assert_eq!(body["to_date"], "2026-04-30");
    let reps = body["deals_by_rep"].as_array().unwrap();
    assert_eq!(reps.len(), 1);
    let dina = &reps[0];
    assert_eq!(dina["rep_id"], 107);
    assert_eq!(dina["deals_count"], 6);
    assert_eq!(dina["won_count"], 1);
    assert_eq!(dina["lost_count"], 1);
    assert_eq!(dina["won_lost_ratio"], 50.0);
    assert_eq!(dina["pipeline_amount"], 1830.25);
    assert_eq!(dina["avg_amount_won"], 999.0);
    assert!((dina["avg_amount"].as_f64().unwrap() - 575.85).abs() < 0.001);
    let remodel = body["deals_by_stage"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stage| stage["list_id"] == 104)
        .unwrap();
    assert_eq!(
        remodel,
        &json!({ "list_id": 104, "list_name": "Measure", "deals_count": 1, "total_amount": 30.0 })
    );
    assert_eq!(
        body["lost_reasons_by_rep"],
        json!([{ "rep_name": "Dina Dealer", "lost_reason": "Too pricey", "count": 1 }])
    );
    // Hidden groups' stages are not columns.
    let lists: Vec<i64> = common::ids(&body["lists"]);
    assert!(lists.contains(&104) && !lists.contains(&106) && !lists.contains(&102));
    assert!(body["customers_totals"]["total"].as_i64().unwrap() > 0);
    assert!(
        body["customers_deals"]
            .as_array()
            .unwrap()
            .iter()
            .all(|deal| deal["sales_rep_name"] == "Dina Dealer")
    );
}

#[test_db::test(seed)]
async fn class_sales_need_quickbooks(pool: MySqlPool) {
    let server = server(pool);
    let body = server
        .get("/v1/qbo/class-sales?start_date=2026-04-01&end_date=2026-04-30")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(
        body,
        json!({ "status": "not_connected", "report": null, "classes": [] })
    );
}
