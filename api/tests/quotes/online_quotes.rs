//! The online quote builder: what it offers, its prices, and a sent quote
//! (the quote, the lead, the email and the team's Telegram or email).

use crate::common;

use api::app;
use api::state::AppState;
use api::telegram::TelegramBots;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

type Sent = Arc<Mutex<Vec<OutgoingEmail>>>;
type Chats = Arc<Mutex<Vec<Value>>>;

async fn send_email(State(sent): State<Sent>, Json(email): Json<OutgoingEmail>) -> Json<Value> {
    sent.lock().unwrap().push(email);
    Json(json!({ "message_id": "<q@ses.test>" }))
}

/// The API with a mail sender and a Telegram that record what they get.
async fn server(pool: &MySqlPool) -> (TestServer, Sent, Chats) {
    let sent = Sent::default();
    let mail = Router::new()
        .route("/email/send", post(send_email))
        .with_state(sent.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mail_base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, mail).await.unwrap() });

    let chats = Chats::default();
    let recorded = chats.clone();
    let telegram = Router::new().route(
        "/{bot}/sendMessage",
        post(
            move |AxumPath(bot): AxumPath<String>, Json(body): Json<Value>| {
                let recorded = recorded.clone();
                async move {
                    let mut list = recorded.lock().unwrap();
                    list.push(json!({ "bot": bot, "body": body }));
                    Json(json!({ "ok": true, "result": { "message_id": 700 + list.len() } }))
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let telegram_base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, telegram).await.unwrap() });

    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_app_url("https://app.acme.test")
        .with_webhooks(WebhooksClient::new(Some(&mail_base), Some("webhooks-key")))
        .with_telegram(TelegramBots::new(
            Some("111:leads"),
            Some("222:notes"),
            &telegram_base,
        ));
    (TestServer::new(app(state)), sent, chats)
}

/// Acme's default price list: material from the stones' prices, $10/sq ft
/// fabrication, sink and cooktop cutouts, one edge profile, an extra every
/// quote includes, and a 7.25% tax rate.
async fn price_list(server: &TestServer, pool: &MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO quote_tax_rates (id, company_id, name, sequence, tax_pct) VALUES (77, 100, 'Marion', 1, 7.25);
         INSERT INTO quote_payment_terms (company_id, name, sequence, terms) VALUES (100, 'Half down', 1, '50% down');
         UPDATE company SET subdomain = 'acme', logo_url = 'https://img.example/acme-logo.png' WHERE id = 100;",
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
        "arrProducts": [],
        "arrFabricationCharges": [{ "materialFilter": {}, "enabled": 1, "price": 10 }],
        "arrSinkUndermounts": [{ "materialFilter": {}, "arrRanges": [{ "maxDimension": null, "price": 150 }] }],
        "arrCooktops": [{ "materialFilter": {}, "arrRanges": [{ "maxDimension": null, "price": 100 }] }],
        "arrEdgeProfileSets": [{ "materialFilter": {}, "arrEdgeProfiles": [{ "name": "Eased", "price": 0 }] }],
        "defaultCooktopCutout": { "cutoutWidth": 30, "cutoutDepth": 20, "burners": 5 },
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
}

fn edge(kind: &str) -> Value {
    json!({ "type": kind })
}

/// A straight 144" × 24" counter against a wall (its back edge).
fn straight(cutouts: Value) -> Value {
    json!({
        "id": "counter-1", "x": 0, "y": 0, "angle": 0,
        "runs": [{ "length": 144, "depth": 24, "turn": 0, "cutouts": cutouts }],
        "edges": [edge("unfinished"), edge("finished"), edge("finished"), edge("finished")],
    })
}

fn black() -> Value {
    json!({ "product": "Granite", "color": "Absolute Black" })
}

#[test_db::test(seed)]
async fn the_builder_offers_stones_in_stock_with_photos_and_sinks(pool: MySqlPool) {
    let (server, _, _) = server(&pool).await;
    // No price list yet: the builder is off.
    let setup = server.get("/v1/public/companies/100/online-quote").await;
    setup.assert_status_ok();
    let setup = setup.json::<Value>();
    assert_eq!(setup["available"], false);
    assert_eq!(setup["company_name"], "Acme Stone");
    assert_eq!(setup["colors"], json!([]));

    price_list(&server, &pool).await;
    let setup = server
        .get("/v1/public/companies/100/online-quote")
        .await
        .json::<Value>();
    assert_eq!(setup["available"], true);
    assert_eq!(setup["logo_url"], "https://img.example/acme-logo.png");
    assert_eq!(setup["default_depth"], 25.5);
    assert_eq!(
        setup["cooktop"],
        json!({ "width": 30.0, "depth": 20.0, "burners": 5 })
    );
    let colors: Vec<String> = setup["colors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            format!(
                "{} {}",
                c["product"].as_str().unwrap(),
                c["color"].as_str().unwrap()
            )
        })
        .collect();
    assert!(
        colors.contains(&"Granite Absolute Black".to_string()),
        "{colors:?}"
    );
    assert!(
        colors.contains(&"Quartz Zebra Quartz".to_string()),
        "{colors:?}"
    );
    // Hidden, deleted, without a photo, sold out, unpriced, another company's.
    for missing in [
        "Granite Hidden Granite",
        "Granite Deleted Granite",
        "Granite No Image Granite",
        "Marble Sold Out Marble",
        "Quartz Calacatta Gold",
        "Granite Other Co Granite",
    ] {
        assert!(!colors.contains(&missing.to_string()), "{missing} offered");
    }
    let black = setup["colors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["color"] == "Absolute Black")
        .unwrap();
    assert_eq!(black["url"], "https://img.example/absolute-black.jpg");
    assert_eq!(black["icon"], "https://img.example/absolute-black-icon.jpg");
    assert!(black.get("price").is_none() && black.get("stockPrice").is_none());

    let sink = setup["sinks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "Undermount 3219")
        .unwrap()
        .clone();
    assert_eq!(
        sink,
        json!({
            "name": "Undermount 3219", "sink_type": "undermount", "sink_shape": "rectangle",
            "width": 32.0, "depth": 19.0, "faucet_holes": 0, "url": "https://img.example/undermount.jpg",
            "icon": "https://img.example/undermount-icon.jpg",
        })
    );
    let sinks: Vec<&str> = setup["sinks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(!sinks.contains(&"Composite Black"), "not on display");
    assert!(!sinks.contains(&"Other Co Sink"));

    server
        .get("/v1/public/companies/999/online-quote")
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn the_builder_prices_each_color(pool: MySqlPool) {
    let (server, _, _) = server(&pool).await;
    price_list(&server, &pool).await;
    let path = "/v1/public/companies/100/online-quote/prices";
    // 24 sq ft × $75 + $10 fabrication + the $75 extra + 7.25% tax on the stone.
    let res = server
        .post(path)
        .json(&json!({ "pieces": [straight(json!([]))], "colors": [black()] }))
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({ "prices": [{ "product": "Granite", "color": "Absolute Black", "total": 2245.5 }] })
    );

    // A sink model from the list and a cooktop add their cutouts and the sink
    // ($199, taxed: $14.43).
    let res = server
        .post(path)
        .json(&json!({
            "pieces": [straight(json!([
                { "id": "sink-1", "kind": "sink", "anchorCorner": 0, "along": 40, "across": 12, "sinkModel": "undermount 3219" },
                { "id": "cook-1", "kind": "cooktop", "anchorCorner": 0, "along": 100, "across": 12 },
            ]))],
            "colors": [black()],
        }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["prices"][0]["total"], 2708.93);

    for (body, why) in [
        (
            json!({ "pieces": [straight(json!([]))], "colors": [{ "product": "Granite", "color": "Hidden Granite" }] }),
            "a color it does not offer",
        ),
        (
            json!({ "pieces": [straight(json!([]))], "colors": [] }),
            "no color",
        ),
        (
            json!({ "pieces": [straight(json!([]))], "colors": [black(), black()] }),
            "the same color twice",
        ),
        (json!({ "pieces": [], "colors": [black()] }), "no piece"),
        (
            json!({ "pieces": [{
                "id": "c", "x": 0, "y": 0, "angle": 0,
                "runs": [{ "length": 100, "depth": 25, "turn": 0 }, { "length": 80, "depth": 25, "turn": 2 }],
                "edges": [edge("finished"), edge("finished"), edge("finished"), edge("finished"), edge("finished"), edge("finished")],
            }], "colors": [black()] }),
            "a corner that is not square",
        ),
        (
            json!({ "pieces": [{
                "id": "c", "x": 0, "y": 0, "angle": 0,
                "runs": [{ "length": 100, "depth": 25, "turn": 0 }],
                "edges": [{ "type": "finished", "bumps": [{ "width": 10, "depth": 2 }] }, edge("finished"), edge("finished"), edge("finished")],
            }], "colors": [black()] }),
            "a bump-out",
        ),
        (
            json!({ "pieces": [{
                "id": "c", "x": 0, "y": 0, "angle": 0,
                "runs": [{ "length": 100, "depth": 25, "turn": 0 }],
                "edges": [edge("finished"), edge("finished"), edge("finished")],
            }], "colors": [black()] }),
            "edges that do not match",
        ),
        (
            json!({ "pieces": [straight(json!([
                { "id": "s", "kind": "sink", "anchorCorner": 0, "along": 40, "across": 12, "sinkModel": "Composite Black" },
            ]))], "colors": [black()] }),
            "a sink that is not on display",
        ),
        (
            json!({ "pieces": [straight(json!([]))], "colors": [black()], "priceOverrides": {} }),
            "an unknown field",
        ),
    ] {
        let res = server.post(path).json(&body).await;
        assert_eq!(res.status_code(), StatusCode::BAD_REQUEST, "{why}");
    }
}

/// The pieces exactly as the builder page sends them (`toWire`): an L with a
/// 4" backsplash, a cooktop on its left run and the customer's own sink on
/// its back run, and an island under it.
#[test_db::test(seed)]
async fn the_builder_pages_pieces_price(pool: MySqlPool) {
    let (server, _, _) = server(&pool).await;
    price_list(&server, &pool).await;
    let splash = json!({ "type": "splash", "splashHeight": 4 });
    let finished = json!({ "type": "finished", "splashHeight": 0 });
    let res = server
        .post("/v1/public/companies/100/online-quote/prices")
        .json(&json!({
            "pieces": [
                {
                    "id": "counter-k83qkhb9", "x": 12.75, "y": 96, "angle": -4,
                    "runs": [
                        { "length": 96, "depth": 25.5, "turn": 0, "cutouts": [
                            { "id": "cooktop-ijkmwnbu", "kind": "cooktop", "anchorCorner": 0, "along": 60.75,
                              "across": 12.75, "sinkModel": null, "width": 30, "depth": 20 },
                        ] },
                        { "length": 132, "depth": 25.5, "turn": 4, "cutouts": [
                            { "id": "sink-itl1aqri", "kind": "sink", "anchorCorner": 0, "along": 78.75,
                              "across": 12.75, "sinkModel": null, "width": 30, "depth": 18 },
                        ] },
                    ],
                    "edges": [splash, splash, finished, finished, finished, finished],
                },
                {
                    "id": "counter-island1", "x": 0, "y": 145.5, "angle": 0,
                    "runs": [{ "length": 84, "depth": 36, "turn": 0, "cutouts": [] }],
                    "edges": [finished, finished, finished, finished],
                },
            ],
            "colors": [black(), { "product": "Quartz", "color": "Zebra Quartz" }],
        }))
        .await;
    res.assert_status_ok();
    let prices = res.json::<Value>()["prices"].clone();
    assert_eq!(prices.as_array().unwrap().len(), 2);
    for price in prices.as_array().unwrap() {
        assert!(price["total"].as_f64().unwrap() > 0.0, "{price}");
    }
}

fn quote_body(email: &str, phone: &str) -> Value {
    json!({
        "name": "Jane Newman", "email": email, "phone": phone,
        "pieces": [straight(json!([]))],
        "colors": [black(), { "product": "Quartz", "color": "Zebra Quartz" }],
    })
}

#[test_db::test(seed)]
async fn a_new_customer_becomes_a_lead_for_the_managers(pool: MySqlPool) {
    let (server, sent, chats) = server(&pool).await;
    price_list(&server, &pool).await;
    let res = server
        .post("/v1/public/companies/100/online-quotes")
        .json(&quote_body(" jane@new.example ", "(317) 555-0199"))
        .await;
    res.assert_status(StatusCode::CREATED);
    let body = res.json::<Value>();
    assert_eq!(body["email"], "jane@new.example");
    assert_eq!(body["emailed"], true);
    assert_eq!(body["prices"][0]["total"], 2245.5);

    let (customer_id, source, referral, form, sales_rep): (
        i32,
        String,
        String,
        String,
        Option<i32>,
    ) = sqlx::query_as(
        "SELECT c.id, c.source, c.referral_source, c.form_name, c.sales_rep FROM customers c
          JOIN customers_emails ce ON ce.id = c.email_id
         WHERE ce.email = 'jane@new.example'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (source.as_str(), referral.as_str(), form.as_str(), sales_rep),
        ("leads", "website", "online_quote", None)
    );

    // An ordinary quote with no sales rep, in the employee quotes.
    let quotes = server
        .get("/v1/quotes")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    let row = quotes["quotes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["customer_name"] == "Jane Newman")
        .unwrap()
        .clone();
    assert_eq!(row["sales_rep"], "");
    assert_eq!(row["status"], "active");
    assert_eq!(row["current_revision"], 1);
    let quote_id = row["id"].as_i64().unwrap();
    let quote = server
        .get(&format!("/v1/quotes/{quote_id}"))
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(quote["quote"]["salesperson_id"], Value::Null);
    assert_eq!(quote["quote"]["email"], "jane@new.example");
    assert_eq!(quote["quote"]["phone"], "(317) 555-0199");
    assert_eq!(quote["quote"]["tax_pct"], 7.25);
    assert_eq!(quote["quote"]["terms"]["terms"], "50% down");
    assert_eq!(quote["doc"]["colorOptions"].as_array().unwrap().len(), 2);
    assert_eq!(quote["doc"]["items"][0]["name"], "Haul away");
    assert_eq!(
        quote["revisions"][0]["notes"],
        "Made by the customer with the online quote"
    );
    assert_eq!(quote["emails"][0]["recipients"], "jane@new.example");

    // The customer's email lists the colors with no price and links to the
    // customer's own page, where the quote prices as it was shown.
    let email = sent.lock().unwrap()[0].clone();
    assert_eq!(email.to, vec!["jane@new.example"]);
    assert_eq!(email.from_name.as_deref(), Some("Acme Stone"));
    assert_eq!(email.reply_to, vec!["manager@acme.test".to_string()]);
    assert_eq!(email.subject, "Acme Stone got your countertop details");
    assert!(
        email.body.contains("Granite Absolute Black"),
        "{}",
        email.body
    );
    assert!(!email.body.contains('$'), "{}", email.body);
    let (view, company_id): (String, i32) =
        sqlx::query_as("SELECT BIN_TO_UUID(view_id), company_id FROM customers WHERE id = ?")
            .bind(customer_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        email.body.contains(&format!(
            "https://acme.app.acme.test/customer/{company_id}/{view}?quote={quote_id}"
        )),
        "{}",
        email.body
    );
    let token = quote["quote"]["portal_token"].as_str().unwrap();
    let portal = server
        .get(&format!("/v1/public/quote-portal/{token}"))
        .await
        .json::<Value>();
    assert_eq!(portal["prices"][0]["total"], 2245.5);

    // Mark (the sales manager) gets the lead with a button to assign it to Rita.
    let chats = chats.lock().unwrap().clone();
    assert_eq!(chats.len(), 1, "{chats:?}");
    assert_eq!(chats[0]["bot"], "bot111:leads");
    let message = &chats[0]["body"];
    assert_eq!(message["chat_id"], 900_101);
    let text = message["text"].as_str().unwrap();
    assert!(
        text.starts_with("New lead from the online quote\nName: Jane Newman\n"),
        "{text}"
    );
    assert!(text.contains("Granite Absolute Black: $2,245.50"), "{text}");
    assert!(text.ends_with("Choose a salesperson."), "{text}");
    let rita: i32 = sqlx::query_scalar(
        "SELECT id FROM users_positions WHERE user_id = 100 AND position_id = 1 AND company_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let keyboard = &message["reply_markup"]["inline_keyboard"];
    assert_eq!(
        keyboard[0],
        json!([{ "text": "Rita Rep: 0", "callback_data": format!("assign:{customer_id}:{rita}") }])
    );
    assert_eq!(
        keyboard[1],
        json!([{ "text": "Open the quote", "url": format!("https://acme.app.acme.test/employee/quote/{quote_id}") }])
    );
    let kept: Vec<(i64, i32)> = sqlx::query_as(
        "SELECT chat_id, message_id FROM telegram_lead_messages WHERE customer_id = ?",
    )
    .bind(customer_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(kept, vec![(900_101, 701)]);
}

#[test_db::test(seed)]
async fn a_known_customer_goes_to_their_sales_rep(pool: MySqlPool) {
    let (server, sent, chats) = server(&pool).await;
    price_list(&server, &pool).await;
    let customers_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM customers")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Alice, by an email she has; her rep Rita has no Telegram: Rita is emailed.
    server
        .post("/v1/public/companies/100/online-quotes")
        .json(&quote_body("ALICE@example.com", "555-000-1234"))
        .await
        .assert_status(StatusCode::CREATED);
    let (customer, salesperson, sales_rep, name): (i32, Option<i32>, String, String) =
        sqlx::query_as(
            "SELECT customer_id, salesperson_id, sales_rep, quote_name FROM quotes ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (customer, salesperson, sales_rep.as_str(), name.as_str()),
        (1000, Some(100), "Rita Rep", "Alice Lead")
    );
    let mail = sent.lock().unwrap().clone();
    assert_eq!(mail.len(), 2);
    assert_eq!(mail[0].reply_to, vec!["rep@acme.test".to_string()]);
    assert_eq!(mail[1].to, vec!["rep@acme.test"]);
    assert_eq!(mail[1].subject, "Jane Newman finished the online quote");
    assert!(
        mail[1].body.contains("/employee/quote/"),
        "{}",
        mail[1].body
    );
    assert!(chats.lock().unwrap().is_empty());

    // Erin, by her phone; her rep Mark has Telegram.
    server
        .post("/v1/public/companies/100/online-quotes")
        .json(&quote_body("erin.new@example.com", "+1 317 555 0105"))
        .await
        .assert_status(StatusCode::CREATED);
    let chats = chats.lock().unwrap().clone();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0]["body"]["chat_id"], 900_101);
    let text = chats[0]["body"]["text"].as_str().unwrap();
    assert!(
        text.starts_with("Jane Newman finished the online quote.\n"),
        "{text}"
    );
    assert_eq!(
        chats[0]["body"]["reply_markup"]["inline_keyboard"][0][0]["text"],
        "Open the quote"
    );
    let salesperson: Option<i32> =
        sqlx::query_scalar("SELECT salesperson_id FROM quotes ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(salesperson, Some(101));

    // Neither made a new customer.
    let customers_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM customers")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(customers_after, customers_before);
}

#[test_db::test(seed)]
async fn bots_and_floods_are_turned_away(pool: MySqlPool) {
    let (server, sent, chats) = server(&pool).await;
    price_list(&server, &pool).await;
    let path = "/v1/public/companies/100/online-quotes";

    // The hidden field filled: answered as sent, nothing saved.
    let mut bot = quote_body("bot@spam.example", "3175550199");
    bot["website"] = json!("http://spam.example");
    server
        .post(path)
        .json(&bot)
        .await
        .assert_status(StatusCode::CREATED);
    let quotes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quotes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(quotes, 0);
    assert!(sent.lock().unwrap().is_empty() && chats.lock().unwrap().is_empty());

    for (name, email, phone) in [
        ("", "jane@new.example", "3175550199"),
        ("Jane", "jane@new", "3175550199"),
        ("Jane", "jane@new.example", "555-0199"),
        ("Jane", "jane@new.example", "3172131231231231231"),
    ] {
        let mut body = quote_body(email, phone);
        body["name"] = json!(name);
        server
            .post(path)
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    // Three a day for one address.
    for _ in 0..3 {
        server
            .post(path)
            .json(&quote_body("jane@new.example", "3175550199"))
            .await
            .assert_status(StatusCode::CREATED);
    }
    server
        .post(path)
        .json(&quote_body("Jane@New.example", "3175550199"))
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
}

#[test_db::test(seed)]
async fn the_customer_adds_the_extras_the_company_offers(pool: MySqlPool) {
    let (server, _, _) = server(&pool).await;
    price_list(&server, &pool).await;
    // Off by default: no Extras step.
    let setup = server.get("/v1/public/companies/100/online-quote").await;
    assert_eq!(setup.json::<Value>()["extras"], json!([]));
    sqlx::raw_sql(
        "INSERT INTO online_quote_options (company_id, name, sequence, enabled) VALUES
           (100, 'Stone tear out', 1, 1), (100, 'Haul away', 2, 1), (100, '10 year sealer', 3, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let setup = server.get("/v1/public/companies/100/online-quote").await;
    assert_eq!(
        setup.json::<Value>()["extras"],
        json!(["Stone tear out", "Haul away"])
    );

    // One that is off is turned away.
    let mut body = quote_body("jane@new.example", "(317) 555-0199");
    body["extras"] = json!(["10 year sealer"]);
    server
        .post("/v1/public/companies/100/online-quotes")
        .json(&body)
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // "Haul away" is already on every quote; "Stone tear out" has no price
    // list extra, so it goes on unpriced for the team.
    body["extras"] = json!(["stone tear out", "Haul away"]);
    let res = server
        .post("/v1/public/companies/100/online-quotes")
        .json(&body)
        .await;
    res.assert_status(StatusCode::CREATED);
    let doc: String = sqlx::query_scalar(
        "SELECT CAST(draft_drawing_json AS CHAR) FROM quotes WHERE email = 'jane@new.example'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let doc: Value = serde_json::from_str(&doc).unwrap();
    let items: Vec<(&str, &str)> = doc["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["kind"].as_str().unwrap(), i["name"].as_str().unwrap()))
        .collect();
    assert_eq!(
        items,
        [("priceList", "Haul away"), ("misc", "Stone tear out")]
    );
}
