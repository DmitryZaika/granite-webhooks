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

#[test_db::test(seed)]
async fn quote_settings_default_then_save(pool: MySqlPool) {
    let server = server(pool);
    let settings: Value = server
        .get("/v1/quote-settings")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        settings,
        json!({ "units": "in", "rounding": 0.0625, "default_depth": 25.5 })
    );
    server
        .put("/v1/quote-settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "rounding": 0.25, "default_depth": 0 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .put("/v1/quote-settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "rounding": 0.25, "default_depth": 26.123 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let settings: Value = server
        .get("/v1/quote-settings")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(settings["rounding"], 0.25);
    assert_eq!(settings["default_depth"], 26.12);
}

#[test_db::test(seed)]
async fn company_quote_email_templates_stay_numbered(pool: MySqlPool) {
    let server = server(pool);
    for name in ["First", "Second"] {
        server
            .post("/v1/quote-email-templates")
            .with_session(sessions::ADMIN)
            .json(&json!({ "name": name, "subject": "Your quote", "body": "Hi", "sequence": 9 }))
            .await
            .assert_status(StatusCode::CREATED);
    }
    let list: Value = server
        .get("/v1/quote-email-templates")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(list[0]["name"], "First");
    assert_eq!(list[0]["sequence"], 1);
    assert_eq!(list[1]["sequence"], 2);
    assert_eq!(list[0]["body"], "Hi\n\n[PDFLink]");
    let second = list[1]["id"].as_i64().unwrap();
    server
        .put(&format!("/v1/quote-email-templates/{second}"))
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "name": "Second", "subject": "Quote", "body": "[QuoteLink]", "sequence": 1 }),
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list: Value = server
        .get("/v1/quote-email-templates")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(list[0]["name"], "Second");
    assert_eq!(list[0]["body"], "[QuoteLink]");
    server
        .delete(&format!("/v1/quote-email-templates/{second}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list: Value = server
        .get("/v1/quote-email-templates")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["sequence"], 1);
}

#[test_db::test(seed)]
async fn online_quote_options_start_with_three_defaults_all_off(pool: MySqlPool) {
    let server = server(pool);
    server
        .get("/v1/online-quote-options")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let list: Value = server
        .get("/v1/online-quote-options")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let rows: Vec<(&str, bool)> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row["name"].as_str().unwrap(), row["enabled"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        rows,
        [("Laminate tear out", false), ("Stone tear out", false), ("10 year sealer", false)]
    );
    let laminate = list[0]["id"].as_i64().unwrap();
    let sealer = list[2]["id"].as_i64().unwrap();
    server
        .delete(&format!("/v1/online-quote-options/{laminate}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .put(&format!("/v1/online-quote-options/{sealer}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "10 year sealer", "sequence": 3, "enabled": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .post("/v1/online-quote-options")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": " Plumbing ", "sequence": 4 }))
        .await
        .assert_status(StatusCode::CREATED);
    // The deleted default does not come back.
    let list: Value = server
        .get("/v1/online-quote-options")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let rows: Vec<(&str, bool)> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row["name"].as_str().unwrap(), row["enabled"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        rows,
        [("Stone tear out", false), ("10 year sealer", true), ("Plumbing", false)]
    );
}
