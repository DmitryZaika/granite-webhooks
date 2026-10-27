//! One quote: creating it, the page and the editor, selling, emailing, and
//! the customer's link.

use crate::common;

use api::app;
use api::state::AppState;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

type Sent = Arc<Mutex<Vec<OutgoingEmail>>>;

async fn send_email(State(sent): State<Sent>, Json(email): Json<OutgoingEmail>) -> Json<Value> {
    sent.lock().unwrap().push(email);
    Json(json!({ "message_id": "<q@ses.test>" }))
}

async fn server(pool: &MySqlPool) -> (TestServer, Sent) {
    let sent = Sent::default();
    let router = Router::new()
        .route("/email/send", post(send_email))
        .with_state(sent.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_app_url("https://app.acme.test")
        .with_webhooks(WebhooksClient::new(Some(&base), Some("webhooks-key")));
    (TestServer::new(app(state)), sent)
}

/// A price list with Granite (Black is sold out, Ubatuba is not) and an
/// extra every new quote includes; a tax rate and payment terms.
async fn price_list(server: &TestServer, pool: &MySqlPool) -> i64 {
    sqlx::raw_sql(
        "INSERT INTO quote_tax_rates (id, company_id, name, sequence, tax_pct) VALUES (77, 100, 'Marion', 1, 7.25);
         INSERT INTO quote_payment_terms (company_id, name, sequence, terms) VALUES (100, 'Half down', 1, '50% down');",
    )
    .execute(pool)
    .await
    .unwrap();
    let res = server
        .post("/v1/quote-price-lists")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Retail" }))
        .await;
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    let info = json!({
        "units": "in", "currency": "$", "expirationDays": 14, "lineItemPriceRounding": 2,
        "defaultTaxRateId": 77,
        "arrProducts": [{ "name": "Granite", "allowDiscount": 1, "arrVariants": [
            { "name": "Black", "price": 50, "soldOut": true }, { "name": "Ubatuba", "price": 40 },
        ] }],
        "arrOtherItems": [{ "name": "Haul away", "alwaysIncludeOnQuote": 1, "arrOptions": [{ "name": "Option 1", "flatPrice": 75 }] }],
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
    id
}

fn doc(color: &str) -> Value {
    json!({
        "version": 2,
        "areas": [{ "id": "area-1", "name": "Kitchen", "quantity": 1 }],
        "colorOptions": [{ "id": "option-1", "byArea": { "area-1": { "product": "Granite", "color": color } } }],
        "items": [{ "id": "i1", "kind": "misc", "areaId": null, "name": "Sealer", "quantity": 1, "price": 100 }],
    })
}

async fn create(server: &TestServer, body: Value) -> i64 {
    let res = server
        .post("/v1/quotes")
        .with_session(sessions::REP)
        .json(&body)
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json::<Value>()["id"].as_i64().unwrap()
}

#[test_db::test(seed)]
async fn a_new_quote_starts_from_the_price_list_and_the_customer(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    price_list(&server, &pool).await;
    let id = create(&server, json!({ "customer_id": 1000 })).await;
    let quote = server
        .get(&format!("/v1/quotes/{id}"))
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(quote["quote"]["name"], "Alice Lead");
    assert_eq!(quote["quote"]["status"], "draft");
    assert_eq!(quote["quote"]["sales_rep"], "Rita Rep");
    assert_eq!(quote["quote"]["price_list_name"], "Retail");
    assert_eq!(quote["quote"]["tax_pct"], 7.25);
    assert_eq!(quote["quote"]["terms"]["terms"], "50% down");
    assert_eq!(quote["quote"]["terms"]["remaining"], "Balance Due");
    assert_eq!(quote["quote"]["phone"], "317-555-0100");
    assert_eq!(quote["doc"]["items"][0]["name"], "Haul away");
    assert_eq!(quote["doc"]["areas"][0]["name"], "Area #1");
    assert_eq!(quote["price_list"]["arrProducts"][0]["name"], "Granite");
    assert!(
        quote["salespeople"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "Rita Rep")
    );
    assert!(quote["sender"].as_str().unwrap().contains('@'));

    server
        .post("/v1/quotes")
        .with_session(sessions::REP)
        .json(&json!({ "customer_id": 2000 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .get(&format!("/v1/quotes/{id}"))
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn a_quote_by_a_non_rep_goes_to_the_customers_rep(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    price_list(&server, &pool).await;
    // Dina has no positions; Alice is Rita's.
    let res = server
        .post("/v1/quotes")
        .with_session(sessions::DEALER)
        .json(&json!({ "customer_id": 1000 }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    let (rep, salesperson, created_by): (String, i32, i32) =
        sqlx::query_as("SELECT sales_rep, salesperson_id, created_by FROM quotes WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (rep.as_str(), salesperson, created_by),
        ("Rita Rep", 100, 107)
    );

    // Bob has no rep, so Dina can't quote him; Rita can.
    let res = server
        .post("/v1/quotes")
        .with_session(sessions::DEALER)
        .json(&json!({ "customer_id": 1001 }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    res.assert_json(&json!({ "error": "Assign this customer to a sales rep first" }));
    create(&server, json!({ "customer_id": 1001 })).await;
}

#[test_db::test(seed)]
async fn the_editor_saves_revisions_and_unsaved_work(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    price_list(&server, &pool).await;
    let id = create(
        &server,
        json!({ "customer_id": 1000, "quote_name": "Kitchen" }),
    )
    .await;
    let path = format!("/v1/quotes/{id}");

    server
        .put(&format!("{path}/drawing"))
        .with_session(sessions::REP)
        .json(&json!({ "doc": { "version": 9 } }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    server
        .put(&format!("{path}/autosave"))
        .with_session(sessions::REP)
        .json(&json!({ "doc": doc("Ubatuba") }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let quote = server
        .get(&path)
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(quote["autosave"]["doc"]["areas"][0]["name"], "Kitchen");

    let res = server
        .put(&format!("{path}/drawing"))
        .with_session(sessions::REP)
        .json(&json!({ "doc": doc("Ubatuba"), "step": 3, "notes": "First" }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["revision"], 1);
    let quote = server
        .get(&path)
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(quote["quote"]["status"], "active");
    assert_eq!(quote["quote"]["current_step"], 3);
    assert_eq!(quote["autosave"], Value::Null);
    assert_eq!(quote["revisions"][0]["notes"], "First");
    let totals: String =
        sqlx::query_scalar("SELECT CAST(totals_json AS CHAR) FROM quote_revisions")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&totals).unwrap()[0]["total"],
        107.25
    );

    let revision = quote["revisions"][0]["id"].as_i64().unwrap();
    let res = server
        .get(&format!("{path}/revisions/{revision}"))
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["doc"]["items"][0]["name"], "Sealer");
    let viewing = server
        .get(&format!("{path}?revision={revision}"))
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(viewing["viewing_revision"], revision);

    server
        .put(&format!("{path}/autosave"))
        .with_session(sessions::REP)
        .json(&json!({ "doc": doc("Ubatuba") }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete(&format!("{path}/autosave"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        server
            .get(&path)
            .with_session(sessions::REP)
            .await
            .json::<Value>()["autosave"],
        Value::Null
    );
}

#[test_db::test(seed)]
async fn a_sold_out_color_still_saves(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    price_list(&server, &pool).await;
    let id = create(
        &server,
        json!({ "customer_id": 1000, "quote_name": "Kitchen" }),
    )
    .await;
    let res = server
        .put(&format!("/v1/quotes/{id}/drawing"))
        .with_session(sessions::REP)
        .json(&json!({ "doc": doc("Black") }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["revision"], 1);
}

#[test_db::test(seed)]
async fn the_page_changes_info_settings_and_prices(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    price_list(&server, &pool).await;
    let id = create(&server, json!({ "customer_id": 1000 })).await;
    let path = format!("/v1/quotes/{id}");
    server
        .patch(&path)
        .with_session(sessions::REP)
        .json(&json!({
            "quote_name": " Island ", "notes": "", "address": " 9 Elm ", "phone_2": "317-555-9999",
            "salesperson_id": 100, "tax_pct": 5, "discount_pct": 10, "discount_amount": -3,
            "expiration_date": "2026-12-31", "terms": { "terms": "Net 30", "mode": "amount" },
        }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let quote = server
        .get(&path)
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(quote["quote"]["quote_name"], "Island");
    assert_eq!(quote["quote"]["notes"], Value::Null);
    assert_eq!(quote["quote"]["address"], "9 Elm");
    assert_eq!(quote["quote"]["phone_2"], "317-555-9999");
    assert_eq!(quote["quote"]["discount_amount"], 0.0);
    assert_eq!(quote["quote"]["expiration_date"], "2026-12-31");
    assert_eq!(quote["quote"]["terms"]["mode"], "amount");
    server
        .patch(&path)
        .with_session(sessions::REP)
        .json(&json!({ "customer_id": 2000 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // Today's prices: a newer revision of the list.
    sqlx::query(
        "INSERT INTO price_list_revisions (price_list_id, revision_number, info_json)
         SELECT price_list_id, 2, JSON_SET(info_json, '$.expirationDays', 99) FROM price_list_revisions",
    )
    .execute(&pool)
    .await
    .unwrap();
    let newest: i64 =
        sqlx::query_scalar("SELECT CAST(MAX(id) AS SIGNED) FROM price_list_revisions")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("UPDATE price_lists SET current_revision_id = ?")
        .bind(newest)
        .execute(&pool)
        .await
        .unwrap();
    server
        .post(&format!("{path}/prices"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let quote = server
        .get(&path)
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(quote["quote"]["price_list_revision_id"], newest);
    assert_eq!(quote["price_list"]["expirationDays"], 99);
}

#[test_db::test(seed)]
async fn a_sale_marks_the_deal_the_user_picked(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    price_list(&server, &pool).await;
    let sell = |id: i64, body: Value| {
        let server = &server;
        async move {
            let path = format!("/v1/quotes/{id}");
            server
                .put(&format!("{path}/drawing"))
                .with_session(sessions::REP)
                .json(&json!({ "doc": doc("Ubatuba") }))
                .await
                .assert_status_ok();
            server
                .post(&format!("{path}/sale"))
                .with_session(sessions::REP)
                .json(&body)
                .await
        }
    };
    let first = create(&server, json!({ "customer_id": 1004 })).await;
    let res = sell(first, json!({ "option": 0 })).await;
    res.assert_status_ok();
    let first_deal = res.json::<Value>()["deal_id"].as_i64().unwrap();

    // A new deal although the customer already has a won one.
    let second = create(&server, json!({ "customer_id": 1004 })).await;
    let res = sell(second, json!({ "option": 0, "new_deal": true })).await;
    res.assert_status_ok();
    let second_deal = res.json::<Value>()["deal_id"].as_i64().unwrap();
    assert_ne!(second_deal, first_deal);

    // The older deal, picked by hand.
    let third = create(&server, json!({ "customer_id": 1004 })).await;
    let res = sell(third, json!({ "option": 0, "deal_id": first_deal })).await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["deal_id"].as_i64(), Some(first_deal));

    // A deal that is not the customer's is refused.
    let fourth = create(&server, json!({ "customer_id": 1004 })).await;
    sell(fourth, json!({ "option": 0, "deal_id": 999_999 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn quotes_are_sold_copied_and_printed(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    price_list(&server, &pool).await;
    let id = create(&server, json!({ "customer_id": 1004 })).await;
    let path = format!("/v1/quotes/{id}");
    server
        .put(&format!("{path}/drawing"))
        .with_session(sessions::REP)
        .json(&json!({ "doc": doc("Ubatuba") }))
        .await
        .assert_status_ok();
    let res = server
        .post(&format!("{path}/sale"))
        .with_session(sessions::REP)
        .json(&json!({ "option": 0 }))
        .await;
    res.assert_status_ok();
    let deal = res.json::<Value>()["deal_id"].as_i64().unwrap();
    let won: (Option<i8>, Option<f64>) =
        sqlx::query_as("SELECT is_won, CAST(amount AS DOUBLE) FROM deals WHERE id = ?")
            .bind(deal)
            .fetch_one(&pool)
            .await
            .unwrap();
    // $100 sealer and 7.25% tax.
    assert_eq!(won, (Some(1), Some(107.25)));
    let quote = server
        .get(&path)
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(quote["quote"]["status"], "converted");
    assert_eq!(quote["quote"]["sold_option"], 0);

    server
        .delete(&format!("{path}/sale"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let is_won: Option<i8> = sqlx::query_scalar("SELECT is_won FROM deals WHERE id = ?")
        .bind(deal)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(is_won, None);

    let res = server
        .post(&format!("{path}/copies"))
        .with_session(sessions::REP)
        .json(&json!({ "quote_name": "Copy", "copy_drawing": true }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let copy = res.json::<Value>()["id"].as_i64().unwrap();
    let copied = server
        .get(&format!("/v1/quotes/{copy}"))
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(copied["quote"]["quote_name"], "Copy");
    assert_eq!(copied["quote"]["status"], "draft");
    assert_eq!(copied["quote"]["address"], Value::Null);
    assert_eq!(copied["doc"]["items"][0]["name"], "Sealer");

    server
        .post(&format!("{path}/prints"))
        .with_session(sessions::REP)
        .json(&json!({ "form_label": "Quote" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let prints: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quote_prints WHERE quote_id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(prints, 1);
}

#[test_db::test(seed)]
async fn a_copy_on_another_list_gets_that_lists_extras(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    price_list(&server, &pool).await;
    // A second list: Cabinets on every new quote, no Haul away.
    let res = server
        .post("/v1/quote-price-lists")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Cabinets" }))
        .await;
    let cabinets = res.json::<Value>()["id"].as_i64().unwrap();
    let info = json!({
        "units": "in", "currency": "$", "expirationDays": 14, "lineItemPriceRounding": 2,
        "arrProducts": [], "arrSinkModels": [],
        "arrOtherItems": [{ "name": "Cabinets", "alwaysIncludeOnQuote": 1, "arrOptions": [{ "name": "Shaker", "flatPrice": 0 }] }],
    });
    sqlx::query(
        "UPDATE price_list_revisions r JOIN price_lists pl ON pl.current_revision_id = r.id
            SET r.info_json = ? WHERE pl.id = ?",
    )
    .bind(info.to_string())
    .bind(cabinets)
    .execute(&pool)
    .await
    .unwrap();
    // Starts on Retail with Haul away.
    let id = create(&server, json!({ "customer_id": 1000 })).await;
    let res = server
        .post(&format!("/v1/quotes/{id}/copies"))
        .with_session(sessions::REP)
        .json(&json!({ "quote_name": "Copy", "copy_drawing": true, "price_list_id": cabinets }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let copy = res.json::<Value>()["id"].as_i64().unwrap();
    let copied = server
        .get(&format!("/v1/quotes/{copy}"))
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(copied["quote"]["price_list_name"], "Cabinets");
    let names: Vec<&str> = copied["doc"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Cabinets"]);
}

#[test_db::test(seed)]
async fn emailing_sends_the_link_and_moves_the_deal(pool: MySqlPool) {
    let (server, sent) = server(&pool).await;
    price_list(&server, &pool).await;
    // Erin's open deal is in "Contacted"; the company has a "Got a Quote" stage.
    sqlx::raw_sql(
        "INSERT INTO deals_list (id, name, position, group_id) VALUES (190, 'Contacted', 5, 100), (191, 'Got a Quote', 6, 100);
         UPDATE deals SET list_id = 190 WHERE id = 8002;
         UPDATE company SET subdomain = 'acme' WHERE id = 100;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let id = create(
        &server,
        json!({ "customer_id": 1004, "quote_name": "Erin island" }),
    )
    .await;
    let path = format!("/v1/quotes/{id}");
    server
        .post(&format!("{path}/emails"))
        .with_session(sessions::REP)
        .json(&json!({ "to": [], "subject": "x", "body": "y" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post(&format!("{path}/emails"))
        .with_session(sessions::REP)
        .json(&json!({ "to": ["erin@builders.example; boss@builders.example"], "subject": "Your quote",
                       "body": "Hi <Erin>,\nhere it is: [QuoteLink]" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let email = sent.lock().unwrap()[0].clone();
    assert_eq!(
        email.to,
        vec!["erin@builders.example", "boss@builders.example"]
    );
    assert_eq!(email.reply_to, vec!["rep@acme.test".to_string()]);
    assert!(
        email.body.starts_with(
            "Hi &lt;Erin&gt;,<br/>here it is: <a href=\"https://acme.app.acme.test/quote-portal/"
        ),
        "{}",
        email.body
    );
    let list: i32 = sqlx::query_scalar("SELECT list_id FROM deals WHERE id = 8002")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(list, 191);

    let quote = server
        .get(&path)
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    let email_id = quote["emails"][0]["id"].as_i64().unwrap();
    assert_eq!(
        quote["emails"][0]["recipients"],
        "erin@builders.example, boss@builders.example"
    );
    assert!(
        quote["emails"][0]["body_html"]
            .as_str()
            .unwrap()
            .contains("View your quote")
    );
    let token = quote["quote"]["portal_token"].as_str().unwrap().to_string();

    // The customer opens the link: a view; then the link is switched off.
    let portal = server
        .get(&format!("/v1/public/quote-portal/{token}"))
        .await
        .json::<Value>();
    assert_eq!(portal["inactive"], false);
    assert_eq!(portal["quote"]["name"], "Erin island");
    assert_eq!(
        portal["prices"][0]["other"]["lines"][0]["description"],
        "Haul away"
    );
    assert!(portal.get("price_list").is_none());
    server
        .patch(&format!("{path}/emails/{email_id}"))
        .with_session(sessions::REP)
        .json(&json!({ "active": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let portal = server
        .get(&format!("/v1/public/quote-portal/{token}"))
        .await
        .json::<Value>();
    assert_eq!(portal["inactive"], true);
}

#[test_db::test(seed)]
async fn the_customer_accepts_and_signs(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    price_list(&server, &pool).await;
    let id = create(&server, json!({ "customer_id": 1004 })).await;
    let path = format!("/v1/quotes/{id}");
    server
        .put(&format!("{path}/drawing"))
        .with_session(sessions::REP)
        .json(&json!({ "doc": doc("Ubatuba") }))
        .await
        .assert_status_ok();
    let res = server
        .post(&format!("{path}/portal-token"))
        .with_session(sessions::REP)
        .await;
    let token = res.json::<Value>()["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 48);
    let portal = server
        .get(&format!("/v1/public/quote-portal/{token}"))
        .await
        .json::<Value>();
    assert_eq!(portal["sold_out"], json!([false]));
    assert_eq!(portal["prices"][0]["total"], 107.25);

    server
        .post(&format!("/v1/public/quote-portal/{token}/acceptance"))
        .json(&json!({ "option": 3, "signature": "data:image/png;base64,AAAA" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post(&format!("/v1/public/quote-portal/{token}/acceptance"))
        .json(&json!({ "option": 0, "signature": "data:image/png;base64,AAAA" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let quote = server
        .get(&path)
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    // Signed, but not sold: staff mark it sold themselves.
    assert_eq!(quote["quote"]["status"], "accepted");
    assert!(quote["quote"]["signed_at"].is_string());
    server
        .post(&format!("/v1/public/quote-portal/{token}/acceptance"))
        .json(&json!({ "option": 0, "signature": "data:image/png;base64,AAAA" }))
        .await
        .assert_status(StatusCode::CONFLICT);
    server
        .get("/v1/public/quote-portal/nope")
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn reps_keep_email_templates_of_their_own(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO quote_email_templates (company_id, name, subject, body, sequence) VALUES (100, 'Company', 'S', 'B [PDFLink]', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let res = server
        .post("/v1/me/quote-email-templates")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Mine", "subject": "Hello", "body": "Hi" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let mine = res.json::<Value>()["id"].as_i64().unwrap();
    let list = server
        .get("/v1/me/quote-email-templates")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(list["ordering_mode"], "automatic");
    assert_eq!(list["templates"][1]["personal"], true);
    assert_eq!(list["templates"][1]["body"], "Hi\n\n[PDFLink]");
    assert_eq!(list["default_id"], list["templates"][0]["id"]);
    // Someone else's view never has it.
    let other = server
        .get("/v1/me/quote-email-templates")
        .with_session(sessions::MANAGER)
        .await
        .json::<Value>();
    assert_eq!(other["templates"].as_array().unwrap().len(), 1);
    server
        .put(&format!("/v1/me/quote-email-templates/{mine}"))
        .with_session(sessions::MANAGER)
        .json(&json!({ "name": "Taken", "subject": "x" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete(&format!("/v1/me/quote-email-templates/{mine}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
}
