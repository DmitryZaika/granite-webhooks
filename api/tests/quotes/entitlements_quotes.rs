//! The Quotes add-on: a subscription company without it gets 403 `addon`
//! `quotes` on every quote route and the online quote builder, while legacy
//! companies and companies with the add-on work as before. Quote portal links
//! stay open for quotes already sent.

use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const REFUSAL: &str = r#"{"error":"Quotes is an add-on for your plan.","addon":"quotes"}"#;

async fn quotes(pool: &MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO customers (id, name, phone, company_id, source, created_date)
           VALUES (20150, 'Pat NoQuotes', '317-555-0150', 20101, 'leads', '2026-01-15 10:00:00');
         INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep,
                             company_id, created_by, status, portal_token)
           VALUES (9801, 1000, 'Legacy kitchen', 'quote', '2026-01-02 10:00:00', 'Rep', 100, 100,
                   'active', 'portal-legacy-9801'),
                  (9802, 20150, 'Sent kitchen', 'quote', '2026-01-02 10:00:00', 'Cora', 20101, 20102,
                   'active', 'portal-no-quotes-9802');",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn legacy_companies_keep_every_quote_route(pool: MySqlPool) {
    let server = server(pool);
    for path in ["/v1/quotes", "/v1/quote-tax-rates", "/v1/commission-rules"] {
        server
            .get(path)
            .with_session(sessions::ADMIN)
            .await
            .assert_status_ok();
    }
}

#[test_db::test(seed)]
async fn a_plan_without_quotes_is_refused_on_quote_routes(pool: MySqlPool) {
    let server = server(pool);
    for path in [
        "/v1/quotes",
        "/v1/quote-price-lists",
        "/v1/quote-tax-rates",
        "/v1/commission-rules",
        "/v1/commissions",
    ] {
        let res = server
            .get(path)
            .with_session(sessions::COMP_SIGNATURES_MANAGER)
            .await;
        res.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(res.text(), REFUSAL, "{path}");
    }
    let res = server
        .post("/v1/quotes")
        .with_session(sessions::COMP_SIGNATURES_MANAGER)
        .json(&json!({ "customer_id": 20150, "quote_name": "Kitchen" }))
        .await;
    res.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(res.text(), REFUSAL);
    // A rep is refused too.
    server
        .get("/v1/quotes")
        .with_session(sessions::COMP_SIGNATURES_REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn a_plan_with_quotes_works_and_stays_in_its_company(pool: MySqlPool) {
    quotes(&pool).await;
    let server = server(pool);
    server
        .get("/v1/quotes")
        .with_session(sessions::QUOTES_ONLY_MANAGER)
        .await
        .assert_status_ok();
    server
        .get("/v1/quotes/9801")
        .with_session(sessions::QUOTES_ONLY_MANAGER)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn the_online_quote_is_off_without_quotes(pool: MySqlPool) {
    let server = server(pool);
    let legacy = server
        .get("/v1/public/companies/100/online-quote")
        .await
        .json::<Value>();
    assert_eq!(legacy["company_name"], "Acme Stone");
    let setup = server.get("/v1/public/companies/20101/online-quote").await;
    setup.assert_status_ok();
    let setup = setup.json::<Value>();
    assert_eq!(setup["available"], false);
    assert_eq!(setup["colors"], json!([]));
    assert_eq!(setup["sinks"], json!([]));
    assert_eq!(setup["extras"], json!([]));
    assert_eq!(setup["company_name"], "Comp Signatures Stone");

    let piece = json!({
        "id": "counter-1", "x": 0, "y": 0, "angle": 0,
        "runs": [{ "length": 144, "depth": 24, "turn": 0, "cutouts": [] }],
        "edges": [{ "type": "unfinished" }, { "type": "finished" },
                  { "type": "finished" }, { "type": "finished" }],
    });
    let colors = json!([{ "product": "Granite", "color": "Absolute Black" }]);
    let res = server
        .post("/v1/public/companies/20101/online-quote/prices")
        .json(&json!({ "pieces": [piece], "colors": colors }))
        .await;
    res.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(res.text(), REFUSAL);
    let res = server
        .post("/v1/public/companies/20101/online-quotes")
        .json(&json!({
            "name": "Jane Newman", "email": "jane@new.example", "phone": "317-555-0199",
            "pieces": [piece], "colors": colors,
        }))
        .await;
    res.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(res.text(), REFUSAL);
}

#[test_db::test(seed)]
async fn quote_portal_links_stay_open_without_quotes(pool: MySqlPool) {
    quotes(&pool).await;
    let server = server(pool);
    for token in ["portal-legacy-9801", "portal-no-quotes-9802"] {
        server
            .get(&format!("/v1/public/quote-portal/{token}"))
            .await
            .assert_status_ok();
    }
}

/// A ready builder for Acme (100): a default price list with prices, as in
/// `online_quotes.rs`.
async fn acme_price_list(server: &axum_test::TestServer, pool: &MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO quote_tax_rates (id, company_id, name, sequence, tax_pct) VALUES (77, 100, 'Marion', 1, 7.25);",
    )
    .execute(pool)
    .await
    .unwrap();
    let id = server
        .post("/v1/quote-price-lists")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Retail" }))
        .await
        .json::<Value>()["id"]
        .as_i64()
        .unwrap();
    let info = json!({
        "units": "in", "currency": "$", "expirationDays": 14, "lineItemPriceRounding": 2,
        "defaultTaxRateId": 77,
        "arrProducts": [],
        "arrFabricationCharges": [{ "materialFilter": {}, "enabled": 1, "price": 10 }],
        "arrSinkUndermounts": [{ "materialFilter": {}, "arrRanges": [{ "maxDimension": null, "price": 150 }] }],
        "arrCooktops": [{ "materialFilter": {}, "arrRanges": [{ "maxDimension": null, "price": 100 }] }],
        "arrEdgeProfileSets": [{ "materialFilter": {}, "arrEdgeProfiles": [{ "name": "Eased", "price": 0 }] }],
        "defaultCooktopCutout": { "cutoutWidth": 30, "cutoutDepth": 20, "burners": 5 },
        "arrOtherItems": [],
        "arrSinkModels": [],
    });
    sqlx::query(
        "UPDATE price_list_revisions r JOIN price_lists pl ON pl.current_revision_id = r.id
            SET r.info_json = ? WHERE pl.id = ?",
    )
    .bind(info.to_string())
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn only_the_plan_turns_a_ready_online_quote_off(pool: MySqlPool) {
    let server = server(pool.clone());
    acme_price_list(&server, &pool).await;
    let available = || async {
        server
            .get("/v1/public/companies/100/online-quote")
            .await
            .json::<Value>()["available"]
            .clone()
    };
    assert_eq!(available().await, true);

    sqlx::query("UPDATE company SET plan_source = 'subscription' WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(available().await, false);

    sqlx::query(
        "INSERT INTO company_addons (company_id, addon_key, source, started_at)
         VALUES (100, 'quotes', 'comp', UTC_TIMESTAMP())",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(available().await, true);
}
