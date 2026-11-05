//! Commissions on signed quotes: rules, settings, the sync and payroll.

use crate::common;

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
        json!({ "payrollWeekday": 5 })
    );
    server
        .put("/v1/commission-settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "payrollWeekday": 9 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        server
            .get("/v1/commission-settings")
            .with_session(sessions::ADMIN)
            .await
            .json::<Value>(),
        json!({ "payrollWeekday": 6 })
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
    assert_eq!(
        res.json::<Value>(),
        json!({ "synced": 1, "qbo": "not_connected" })
    );

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

    // Once paid, it lands on the next payroll Friday with the rep's base pay.
    sqlx::raw_sql(
        "UPDATE quote_commissions SET status = 'paid', paid_date = '2026-10-13', payroll_date = '2026-10-16';
         INSERT INTO commission_base_pay (company_id, user_id, amount, frequency, start_date, end_date)
           VALUES (100, 100, 100, 'weekly', '2026-10-16', '2026-10-16');",
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

#[test_db::test(seed)]
async fn admins_add_paid_installed_jobs_to_the_salary_and_set_amounts(pool: MySqlPool) {
    let server = server(pool.clone());
    signed_quote(&pool).await;
    // Marked sold (no signature), with its install done; and a sheet row.
    sqlx::raw_sql(
        "UPDATE quotes SET signed_at = NULL, status = 'converted' WHERE id = 9701;
         INSERT INTO calendars (id, company_id, name, color, slug) VALUES (8830, 100, 'Installations', 'blue', 'installations');
         INSERT INTO events (title, start_date, end_date, created_user_id, quote_id, calendar_id)
           VALUES ('Install', '2026-01-05 09:00:00', '2026-01-05 12:00:00', 100, 9701, 8830);
         INSERT INTO quote_commissions (company_id, user_id, customer_label, quote_total, commission_amount, status, signed_at)
           VALUES (100, 100, 'Old sale (sheet)', 1000, 50, 'no_invoice', '2025-12-01 12:00:00');",
    )
    .execute(&pool)
    .await
    .unwrap();
    server
        .post("/v1/commissions/sync")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
    let rows = server
        .get("/v1/commissions?scope=company")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>()["rows"]
        .clone();
    let quote_row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["quoteId"] == 9701)
        .unwrap()
        .clone();
    let sheet_row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["quoteId"].is_null())
        .unwrap()
        .clone();
    assert_eq!(quote_row["installedDate"], "2026-01-05");
    assert!(quote_row["payrollDate"].is_null());
    // The CRM's customer; a sheet row goes by its own name.
    assert_eq!(quote_row["customerName"], "Alice Lead");
    assert_eq!(quote_row["customerId"], 1000);
    assert!(quote_row["label"].is_null());
    assert_eq!(sheet_row["customerName"], "Old sale (sheet)");
    assert_eq!(sheet_row["label"], "Old sale (sheet)");
    assert!(sheet_row["customerId"].is_null());
    let quote_id = quote_row["id"].as_i64().unwrap();
    let sheet_id = sheet_row["id"].as_i64().unwrap();

    // Not paid yet: it cannot go into the salary.
    let res = server
        .post(&format!("/v1/commissions/{quote_id}/payroll"))
        .with_session(sessions::ADMIN)
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);

    // Paid to the rep: out of the salary, it goes into today's paycheck;
    // taking it back keeps that payday. Reps cannot mark it.
    server
        .post("/v1/commissions/payouts")
        .with_session(sessions::REP)
        .json(&json!({ "ids": [sheet_id] }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let paid_out = |pool: MySqlPool| async move {
        sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT DATE_FORMAT(paid_out_at, '%Y'), DATE_FORMAT(payroll_date, '%Y-%m-%d')
               FROM quote_commissions WHERE quote_id IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    server
        .post("/v1/commissions/payouts")
        .with_session(sessions::ADMIN)
        .json(&json!({ "ids": [sheet_id] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let today = chrono::Utc::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let (out, payday) = paid_out(pool.clone()).await;
    assert!(out.is_some());
    assert_eq!(payday.as_deref(), Some(today.as_str()));
    server
        .post("/v1/commissions/payouts")
        .with_session(sessions::ADMIN)
        .json(&json!({ "ids": [sheet_id], "paid": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let (out, payday) = paid_out(pool.clone()).await;
    assert!(out.is_none());
    assert_eq!(payday.as_deref(), Some(today.as_str()));
    sqlx::raw_sql("UPDATE quote_commissions SET payroll_date = NULL WHERE quote_id IS NULL;")
        .execute(&pool)
        .await
        .unwrap();

    // Paid long ago and installed: the next payroll Friday from today.
    sqlx::raw_sql("UPDATE quote_commissions SET status = 'paid', paid_date = '2026-01-07' WHERE quote_id = 9701;")
        .execute(&pool)
        .await
        .unwrap();
    let res = server
        .post(&format!("/v1/commissions/{quote_id}/payroll"))
        .with_session(sessions::ADMIN)
        .await;
    res.assert_status_ok();
    let payroll = res.json::<Value>()["payrollDate"]
        .as_str()
        .unwrap()
        .to_string();
    let date = chrono::NaiveDate::parse_from_str(&payroll, "%Y-%m-%d").unwrap();
    assert_eq!(date.format("%a").to_string(), "Fri");
    assert!(date >= chrono::Utc::now().date_naive());
    server
        .post(&format!("/v1/commissions/{quote_id}/payroll"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // The admin's own amounts win, and the sync keeps them.
    server
        .put(&format!("/v1/commissions/{quote_id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "quoteTotal": 900, "commission": 45.5, "payrollDate": payroll }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .put(&format!("/v1/commissions/{sheet_id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "quoteTotal": null, "commission": -1, "payrollDate": null }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .put(&format!("/v1/commissions/{quote_id}"))
        .with_session(sessions::REP)
        .json(&json!({ "commission": 1 }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/commissions/sync")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
    let mine = server
        .get("/v1/commissions")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    let row = mine["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["quoteId"] == 9701)
        .unwrap()
        .clone();
    assert_eq!(row["quoteTotal"], 900.0);
    assert_eq!(row["commission"], 45.5);
    assert_eq!(row["calculatedTotal"], 100.0);
    assert_eq!(row["adjusted"], true);
    assert_eq!(mine["weeks"][0]["payrollDate"], payroll);
    assert_eq!(mine["weeks"][0]["reps"][0]["commission"], 45.5);

    // Only a row with no quote can be deleted.
    server
        .delete(&format!("/v1/commissions/{quote_id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete(&format!("/v1/commissions/{sheet_id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

#[test_db::test(seed)]
async fn a_positions_base_pay_pays_everyone_with_it_unless_they_have_their_own(pool: MySqlPool) {
    let server = server(pool.clone());
    // Every Sales Representative (position 1): $500 on 10/2 and 10/9.
    server
        .post("/v1/commission-base-pay")
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "positionId": 1, "amount": 500, "frequency": "weekly",
                       "startDate": "2026-10-02", "endDate": "2026-10-09" }),
        )
        .await
        .assert_status(StatusCode::CREATED);
    for bad in [
        json!({ "userId": 100, "positionId": 1, "amount": 1, "frequency": "weekly", "startDate": "2026-10-02" }),
        json!({ "amount": 1, "frequency": "weekly", "startDate": "2026-10-02" }),
        json!({ "positionId": 999, "amount": 1, "frequency": "weekly", "startDate": "2026-10-02" }),
    ] {
        server
            .post("/v1/commission-base-pay")
            .with_session(sessions::ADMIN)
            .json(&bad)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    let list = server
        .get("/v1/commission-base-pay")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(list[0]["positionId"], 1);
    assert_eq!(list[0]["userId"], Value::Null);
    assert_eq!(list[0]["rep"], Value::Null);

    // Rita is the company's only sales rep: Gus left, and Mark and Olga are
    // sales reps in another company.
    let reps_on = |weeks: &Value, date: &str| -> Vec<(i64, f64)> {
        weeks
            .as_array()
            .unwrap()
            .iter()
            .filter(|w| w["payrollDate"] == date)
            .flat_map(|w| w["reps"].as_array().unwrap().clone())
            .map(|r| (r["userId"].as_i64().unwrap(), r["base"].as_f64().unwrap()))
            .collect()
    };
    let all = server
        .get("/v1/commissions?scope=company")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(reps_on(&all["weeks"], "2026-10-02"), [(100, 500.0)]);
    assert_eq!(reps_on(&all["weeks"], "2026-10-09"), [(100, 500.0)]);

    // Her own base pay replaces the position's.
    server
        .post("/v1/commission-base-pay")
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "userId": 100, "amount": 700, "frequency": "weekly",
                       "startDate": "2026-10-02", "endDate": "2026-10-02" }),
        )
        .await
        .assert_status(StatusCode::CREATED);
    let mine = server
        .get("/v1/commissions")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(reps_on(&mine["weeks"], "2026-10-02"), [(100, 700.0)]);
    assert!(reps_on(&mine["weeks"], "2026-10-09").is_empty());
}

#[test_db::test(seed)]
async fn admins_set_each_reps_base_pay_schedule(pool: MySqlPool) {
    let server = server(pool.clone());
    // $500 on the 20th of every month, from 9/20 to 10/20.
    let res = server
        .post("/v1/commission-base-pay")
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "userId": 100, "amount": 500, "frequency": "monthly",
                       "startDate": "2026-09-20", "endDate": "2026-10-20" }),
        )
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    for bad in [
        json!({ "userId": 100, "amount": -1, "frequency": "weekly", "startDate": "2026-09-20" }),
        json!({ "userId": 100, "amount": 1, "frequency": "daily", "startDate": "2026-09-20" }),
        json!({ "userId": 100, "amount": 1, "frequency": "weekly", "startDate": "9/20/2026" }),
        json!({ "userId": 100, "amount": 1, "frequency": "weekly", "startDate": "2026-09-20", "endDate": "2026-09-01" }),
        // Another company's user.
        json!({ "userId": 103, "amount": 1, "frequency": "weekly", "startDate": "2026-09-20" }),
    ] {
        server
            .post("/v1/commission-base-pay")
            .with_session(sessions::ADMIN)
            .json(&bad)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    let list = server
        .get("/v1/commission-base-pay")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(list[0]["rep"], "Rita Rep");
    assert_eq!(list[0]["frequency"], "monthly");
    assert_eq!(list[0]["endDate"], "2026-10-20");

    // The rep's paychecks: the base on 9/20 and 10/20, with no commissions.
    let mine = server
        .get("/v1/commissions")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    let dates: Vec<&str> = mine["weeks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["payrollDate"].as_str().unwrap())
        .collect();
    assert_eq!(dates, ["2026-10-20", "2026-09-20"]);
    assert_eq!(mine["weeks"][0]["reps"][0]["base"], 500.0);
    assert_eq!(mine["weeks"][0]["reps"][0]["total"], 500.0);

    // Every two weeks from 10/2, until 10/16.
    server
        .put(&format!("/v1/commission-base-pay/{id}"))
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "userId": 100, "amount": 250, "frequency": "biweekly",
                       "startDate": "2026-10-02", "endDate": "2026-10-16" }),
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let weeks = server
        .get("/v1/commissions")
        .with_session(sessions::REP)
        .await
        .json::<Value>()["weeks"]
        .clone();
    assert_eq!(weeks.as_array().unwrap().len(), 2);
    assert_eq!(weeks[0]["payrollDate"], "2026-10-16");
    server
        .get("/v1/commission-base-pay")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .delete(&format!("/v1/commission-base-pay/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        server
            .get("/v1/commissions")
            .with_session(sessions::REP)
            .await
            .json::<Value>()["weeks"],
        json!([])
    );
}
