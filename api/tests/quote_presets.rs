mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn admins_manage_tax_rates_and_employees_read_them(pool: MySqlPool) {
    let server = server(pool);
    server
        .post("/v1/quote-tax-rates")
        .with_session(sessions::REP)
        .json(&json!({ "name": "County", "tax_pct": 7 }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/quote-tax-rates")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "County", "tax_pct": 101 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let res = server
        .post("/v1/quote-tax-rates")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": " County ", "sequence": 2, "tax_pct": 7.25 }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    server
        .post("/v1/quote-tax-rates")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "State", "tax_pct": 7 }))
        .await
        .assert_status(StatusCode::CREATED);
    server
        .put(&format!("/v1/quote-tax-rates/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Marion", "sequence": 2, "tax_pct": 7.5 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list: Value = server
        .get("/v1/quote-tax-rates")
        .with_session(sessions::REP)
        .await
        .json();
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["State", "Marion"]);
    assert_eq!(list[1]["tax_pct"], 7.5);
    server
        .delete(&format!("/v1/quote-tax-rates/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete(&format!("/v1/quote-tax-rates/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let other: Value = server
        .get("/v1/quote-tax-rates")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(other, json!([]));
}

#[test_db::test(seed)]
async fn admins_manage_payment_terms(pool: MySqlPool) {
    let server = server(pool);
    server
        .post("/v1/quote-payment-terms")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Deposit", "terms": "  " }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let res = server
        .post("/v1/quote-payment-terms")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Deposit", "terms": "50% now" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    server
        .put(&format!("/v1/quote-payment-terms/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Deposit", "terms": "60% now" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list: Value = server
        .get("/v1/quote-payment-terms")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        list,
        json!([{ "id": id, "name": "Deposit", "sequence": 1, "terms": "60% now" }])
    );
}
