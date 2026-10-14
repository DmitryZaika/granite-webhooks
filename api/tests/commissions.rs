//! Commissions on signed quotes: rules, settings, the sync and payroll.

mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

fn doc() -> Value {
    json!({
        "version": 2,
        "areas": [{ "id": "a1", "name": "Kitchen", "quantity": 1 }],
        "colorOptions": [{ "id": "o1", "byArea": {} }],
        "items": [{ "id": "i1", "kind": "misc", "areaId": null, "name": "Sealer", "quantity": 2, "price": 50 }],
    })
}

async fn signed_quote(pool: &MySqlPool) {
    let list = json!({
        "units": "in", "currency": "$", "expirationDays": 30, "lineItemPriceRounding": 2,
        "arrProducts": [], "arrOtherItems": [], "arrSinkModels": [],
    });
    sqlx::query(
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep, company_id,
                             salesperson_id, created_by, status, draft_drawing_json, price_snapshot_json, signed_at)
         VALUES (9701, 1000, 'Kitchen', 'quote', NOW(), 'Rita Rep', 100, 100, 100, 'sold', ?, ?, '2026-10-01 12:00:00')",
    )
    .bind(doc().to_string())
    .bind(list.to_string())
    .execute(pool)
    .await
    .unwrap();
}

async fn add_rule(server: &axum_test::TestServer, rule: Value) -> i64 {
    let res = server
        .post("/v1/commission-rules")
        .with_session(sessions::ADMIN)
        .json(&rule)
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json::<Value>()["id"].as_i64().unwrap()
}

#[test_db::test(seed)]
async fn admins_manage_rules_and_settings(pool: MySqlPool) {
    let server = server(pool.clone());
    let id = add_rule(
        &server,
        json!({ "name": " Stone ", "applies": "material", "rateType": "percent", "rate": 5,
                "materials": [" Granite "], "levelMin": 1, "stop": true }),
    )
    .await;
    let res = server
        .post("/v1/commission-rules")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Bad", "applies": "quote", "rateType": "flat", "rate": -1 }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "Rate cannot be negative");
    server
        .put(&format!("/v1/commission-rules/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Stone", "applies": "material", "rateType": "flat", "rate": 2, "active": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let rules = server
        .get("/v1/commission-rules")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(rules[0]["name"], "Stone");
    assert_eq!(rules[0]["rateType"], "flat");
    assert_eq!(rules[0]["active"], false);
    assert_eq!(rules[0]["materials"], json!([]));
    server
        .delete(&format!("/v1/commission-rules/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        server
            .get("/v1/commission-rules")
            .with_session(sessions::ADMIN)
            .await
            .json::<Value>(),
        json!([])
    );

    assert_eq!(
        server
            .get("/v1/commission-settings")
            .with_session(sessions::ADMIN)
            .await
            .json::<Value>(),
        json!({ "baseWeekly": 0.0, "payrollWeekday": 5 })
    );
    server
        .put("/v1/commission-settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "baseWeekly": -5, "payrollWeekday": 9 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        server
            .get("/v1/commission-settings")
            .with_session(sessions::ADMIN)
            .await
            .json::<Value>(),
        json!({ "baseWeekly": 0.0, "payrollWeekday": 6 })
    );
    server
        .get("/v1/commission-rules")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn the_sync_works_out_signed_quotes_and_payroll_pays_them(pool: MySqlPool) {
    let server = server(pool.clone());
    signed_quote(&pool).await;
    add_rule(
        &server,
        json!({ "name": "Quote", "applies": "quote", "rateType": "percent", "rate": 10 }),
    )
    .await;
    add_rule(
        &server,
        json!({ "name": "Sealer", "applies": "lines", "rateType": "flat", "rate": 5, "lineMatch": "sealer" }),
    )
    .await;
    let res = server
        .post("/v1/commissions/sync")
        .with_session(sessions::ADMIN)
        .await;
    res.assert_status_ok();
    // The company has no QuickBooks: payments are unknown.
    assert_eq!(res.json::<Value>(), json!({ "synced": 1, "qbo": "not_connected" }));

    let mine = server
        .get("/v1/commissions")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    let row = &mine["rows"][0];
    assert_eq!(row["quoteId"], 9701);
    assert_eq!(row["quoteTotal"], 100.0);
    assert_eq!(row["commission"], 20.0);
    assert_eq!(row["status"], "no_invoice");
    assert_eq!(row["signedAt"], "2026-10-01");
    assert_eq!(row["parts"][0]["note"], "Whole quote");
    assert_eq!(row["parts"][1]["note"], "2 - Sealer");
    assert_eq!(mine["weeks"], json!([]));

    // Once paid, it lands on the next payroll Friday with the base.
    sqlx::raw_sql(
        "UPDATE quote_commissions SET status = 'paid', paid_date = '2026-10-13', payroll_date = '2026-10-16';
         INSERT INTO commission_settings (company_id, base_weekly, payroll_weekday) VALUES (100, 100, 5);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let all = server
        .get("/v1/commissions?scope=company")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(all["weeks"][0]["payrollDate"], "2026-10-16");
    assert_eq!(all["weeks"][0]["reps"][0]["total"], 120.0);
    server
        .get("/v1/commissions?scope=company")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);

    let id = all["rows"][0]["id"].as_i64().unwrap();
    server
        .post("/v1/commissions/payouts")
        .with_session(sessions::ADMIN)
        .json(&json!({ "ids": [id] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let paid_out: Option<String> =
        sqlx::query_scalar("SELECT DATE_FORMAT(paid_out_at, '%Y') FROM quote_commissions")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(paid_out.is_some());
}
