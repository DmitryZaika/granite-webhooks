//! `SignWell`'s webhook: the request's status moves forward, the sender is
//! told, and a signed contract sells its quote at the option's price.

mod common;

use api::app;
use api::routes::signwell_webhook::event_hash;
use api::state::{AppState, SignWellWebhookKeys};
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

const TOKEN: &str = "webhook-secret";
const WEBHOOK_ID: &str = "webhook-id";

/// One straight 100" × 25.5" counter (17.7 sq ft) in Granite / Absolute Black.
fn drawing(options: usize) -> Value {
    let option = |i: usize| json!({ "id": format!("option-{i}"), "byArea": { "area-1": { "product": "Granite", "color": "Absolute Black", "edges": [] } } });
    let unfinished = json!({ "type": "unfinished" });
    json!({
        "version": 2,
        "counters": [{
            "id": "c1", "name": "Kitchen", "areaId": "area-1", "x": 0, "y": 0, "angle": 0,
            "runs": [{ "length": 100, "depth": 25.5, "turn": 0, "cutouts": [] }],
            "edges": [unfinished, unfinished, unfinished, unfinished],
            "corners": [[null, null], [null, null]],
        }],
        "areas": [{ "id": "area-1", "name": "Kitchen", "quantity": 1 }],
        "colorOptions": (1..=options).map(option).collect::<Vec<_>>(),
    })
}

fn price_list(variant: &Value) -> Value {
    json!({
        "units": "in", "currency": "$", "expirationDays": 30, "lineItemPriceRounding": 2,
        "arrProducts": [{ "name": "Granite", "allowDiscount": 1, "arrVariants": [variant] }],
        "arrOtherItems": [], "arrSinkModels": [],
    })
}

const FIXTURE: &str = "
INSERT INTO price_lists (id, company_id, name, current_revision_id) VALUES (500, 100, 'Retail', NULL);
INSERT INTO quote_forms (company_id, name, config_json) VALUES (100, 'Agreement', '{\"kind\":\"contract\"}');
";

#[derive(Clone, Default)]
struct Fakes {
    emails: Arc<Mutex<Vec<OutgoingEmail>>>,
}

async fn send_email(State(fakes): State<Fakes>, Json(email): Json<OutgoingEmail>) -> Json<Value> {
    fakes.emails.lock().unwrap().push(email);
    Json(json!({ "message_id": "<n1@ses.test>" }))
}

struct Setup {
    server: TestServer,
    fakes: Fakes,
}

async fn setup(pool: &MySqlPool) -> Setup {
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    let fakes = Fakes::default();
    let router = Router::new()
        .route("/email/send", post(send_email))
        .with_state(fakes.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_webhooks(WebhooksClient::new(Some(&base), Some("webhooks-key")))
        .with_signwell_webhook(SignWellWebhookKeys::new(TOKEN, WEBHOOK_ID))
        .with_signature_app_origin("https://crm.acme.test/");
    Setup {
        server: TestServer::new(app(state)),
        fakes,
    }
}

/// A quote for a customer; `snapshot` is the price list it was made with.
async fn quote(
    pool: &MySqlPool,
    customer: i64,
    deal: Option<i64>,
    doc: &Value,
    snapshot: Option<&Value>,
) -> i64 {
    let revision = sqlx::query(
        "INSERT INTO price_list_revisions (price_list_id, revision_number, info_json)
         SELECT 500, COALESCE(MAX(revision_number), 0) + 1, ? FROM price_list_revisions WHERE price_list_id = 500",
    )
    .bind(price_list(&json!({ "name": "Absolute Black" })).to_string())
    .execute(pool)
    .await
    .unwrap()
    .last_insert_id();
    sqlx::query(
        "INSERT INTO quotes (customer_id, quote_name, quote_type, created_date, sales_rep, company_id, status,
                             deal_id, price_list_id, price_list_revision_id, draft_drawing_json, price_snapshot_json,
                             created_by, updated_at)
         VALUES (?, 'Kitchen', 'quote', NOW(), 'Rita Rep', 100, 'active', ?, 500, ?, ?, ?, 100, NOW())",
    )
    .bind(customer)
    .bind(deal)
    .bind(revision)
    .bind(doc.to_string())
    .bind(snapshot.map(Value::to_string))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query_scalar("SELECT CAST(LAST_INSERT_ID() AS SIGNED)")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn request(
    pool: &MySqlPool,
    customer: i64,
    document: &str,
    filename: &str,
    status: &str,
) -> i64 {
    sqlx::query(
        "INSERT INTO signature_requests (company_id, customer_id, created_by_user_id, filename, file_url, signer_name,
                                         signer_email, status, provider, provider_document_id)
         VALUES (100, ?, 100, ?, 'https://files.test/c.pdf', 'Erin Builder', 'erin@example.com', ?, 'signwell', ?)",
    )
    .bind(customer)
    .bind(filename)
    .bind(status)
    .bind(document)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query_scalar("SELECT CAST(LAST_INSERT_ID() AS SIGNED)")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn event(kind: &str, document: &str) -> Value {
    json!({ "event": { "type": kind, "time": 1_760_000_000 }, "data": { "object": { "id": document } } })
}

impl Setup {
    async fn send(&self, kind: &str, document: &str) -> axum_test::TestResponse {
        self.server
            .post("/v1/webhooks/signwell")
            .add_query_param("token", TOKEN)
            .json(&event(kind, document))
            .await
    }
}

async fn status_of(pool: &MySqlPool, id: i64) -> String {
    sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM signature_requests WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn a_signed_contract_sells_its_quote_and_wins_its_deal(pool: MySqlPool) {
    let setup = setup(&pool).await;
    // Made with Absolute Black at a fixed $50/sq ft: 17.7 × 50 = $885.
    let snapshot = price_list(&json!({ "name": "Absolute Black", "price": 50 }));
    let quote_id = quote(&pool, 1004, Some(8002), &drawing(1), Some(&snapshot)).await;
    let id = request(&pool, 1004, "doc-1", "Contract - Kitchen.pdf", "sent").await;

    setup
        .send("document_completed", "doc-1")
        .await
        .assert_status_ok();
    assert_eq!(status_of(&pool, id).await, "signed");
    let (status, option, deal): (String, Option<i32>, Option<i64>) = sqlx::query_as(
        "SELECT status, customer_accepted_option_id, CAST(deal_id AS SIGNED) FROM quotes WHERE id = ?",
    )
    .bind(quote_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), option, deal),
        ("converted", Some(0), Some(8002))
    );
    let (won, amount): (Option<i8>, Option<f64>) =
        sqlx::query_as("SELECT is_won, CAST(amount AS DOUBLE) FROM deals WHERE id = 8002")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((won, amount), (Some(1), Some(885.0)));

    // The sender hears it, linked to the quote's deal.
    let emails = setup.fakes.emails.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].to, vec!["rep@acme.test".to_string()]);
    assert_eq!(emails[0].subject, "Erin Builder signed Contract - Kitchen");
    assert!(emails[0].transactional);
    assert!(emails[0].body.contains("Your Document Has Been Signed"));
    assert!(
        emails[0]
            .body
            .contains("href=\"https://crm.acme.test/employee/deals/edit/8002/project\"")
    );
    let notice: (i64, Option<i64>, String, String) = sqlx::query_as(
        "SELECT CAST(user_id AS SIGNED), CAST(deal_id AS SIGNED), message, notification_type
           FROM notifications WHERE notification_type = 'signature_signed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        notice,
        (
            100,
            Some(8002),
            "Erin Builder signed Contract - Kitchen".into(),
            "signature_signed".into()
        )
    );

    // Signed is final: a late "viewed" changes nothing and tells no one.
    setup
        .send("document_viewed", "doc-1")
        .await
        .assert_status_ok();
    assert_eq!(status_of(&pool, id).await, "signed");
    assert_eq!(setup.fakes.emails.lock().unwrap().len(), 1);
}

#[test_db::test(seed)]
async fn a_quote_with_no_snapshot_prices_from_today_and_gets_a_new_won_deal(pool: MySqlPool) {
    let setup = setup(&pool).await;
    // Bob's only deal is lost: the sale gets a new one. No snapshot: the
    // revision with today's stock, Absolute Black at its $75 retail price
    // (17.7 × 75 = 1327.5, rounded to the dollar).
    let quote_id = quote(&pool, 1001, None, &drawing(1), None).await;
    request(&pool, 1001, "doc-2", "agreement - kitchen.PDF", "viewed").await;

    setup
        .send("document_signed", "doc-2")
        .await
        .assert_status_ok();
    let (deal, snapshot): (Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT CAST(deal_id AS SIGNED), CAST(price_snapshot_json AS CHAR) FROM quotes WHERE id = ?",
    )
    .bind(quote_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let deal = deal.expect("a deal for the sale");
    let (customer, won, amount, list, user): (i64, Option<i8>, Option<f64>, i64, Option<i64>) = sqlx::query_as(
        "SELECT CAST(customer_id AS SIGNED), is_won, CAST(amount AS DOUBLE), CAST(list_id AS SIGNED),
                CAST(user_id AS SIGNED) FROM deals WHERE id = ?",
    )
    .bind(deal)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (customer, won, amount, list, user),
        (1001, Some(1), Some(1328.0), 100, Some(100))
    );
    let stages: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deal_stage_history WHERE deal_id = ? AND list_id = 100",
    )
    .bind(deal)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stages, 1);
    // The prices it was sold at are kept with the quote.
    let snapshot: Value = serde_json::from_str(&snapshot.expect("snapshot stored")).unwrap();
    assert_eq!(
        snapshot["arrProducts"][0]["arrVariants"][0]["stockPrice"],
        75
    );
}

#[test_db::test(seed)]
async fn a_contract_for_several_options_wins_the_deal_and_waits_for_the_option(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let snapshot = price_list(&json!({ "name": "Absolute Black", "price": 50 }));
    let quote_id = quote(&pool, 1004, Some(8002), &drawing(2), Some(&snapshot)).await;
    request(&pool, 1004, "doc-3", "Contract - Kitchen.pdf", "sent").await;

    setup
        .send("document_completed", "doc-3")
        .await
        .assert_status_ok();
    let (status, option): (String, Option<i32>) =
        sqlx::query_as("SELECT status, customer_accepted_option_id FROM quotes WHERE id = ?")
            .bind(quote_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), option), ("accepted", None));
    let (won, amount): (Option<i8>, Option<f64>) =
        sqlx::query_as("SELECT is_won, CAST(amount AS DOUBLE) FROM deals WHERE id = 8002")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((won, amount), (Some(1), Some(2500.5)));
}

#[test_db::test(seed)]
async fn other_signed_files_are_not_a_sale(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let snapshot = price_list(&json!({ "name": "Absolute Black", "price": 50 }));
    let quote_id = quote(&pool, 1004, Some(8002), &drawing(1), Some(&snapshot)).await;
    let id = request(&pool, 1004, "doc-4", "Template approval.pdf", "sent").await;

    setup
        .send("document_viewed", "doc-4")
        .await
        .assert_status_ok();
    assert_eq!(status_of(&pool, id).await, "viewed");
    // No quote's contract: the notice links to the customer.
    let emails = setup.fakes.emails.lock().unwrap().clone();
    assert_eq!(emails[0].subject, "Erin Builder opened Template approval");
    assert!(
        emails[0]
            .body
            .contains("https://crm.acme.test/employee/customers/info/1004/documents")
    );

    setup
        .send("document_declined", "doc-4")
        .await
        .assert_status_ok();
    assert_eq!(status_of(&pool, id).await, "declined");
    let status: String = sqlx::query_scalar("SELECT status FROM quotes WHERE id = ?")
        .bind(quote_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "active");
    let notices: Vec<String> = sqlx::query_scalar(
        "SELECT notification_type FROM notifications WHERE customer_id = 1004 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(notices, ["signature_viewed", "signature_declined"]);
}

#[test_db::test(seed)]
async fn only_signwell_may_call(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = request(&pool, 1004, "doc-5", "Template approval.pdf", "sent").await;
    let body = event("document_viewed", "doc-5");

    let res = setup.server.post("/v1/webhooks/signwell").json(&body).await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    let res = setup
        .server
        .post("/v1/webhooks/signwell")
        .add_query_param("token", "wrong")
        .json(&body)
        .await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    let mut forged = body.clone();
    forged["event"]["hash"] = event_hash("other-id", "document_viewed", 1_760_000_000.0).into();
    let res = setup
        .server
        .post("/v1/webhooks/signwell")
        .json(&forged)
        .await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(status_of(&pool, id).await, "sent");

    // SignWell's own hash of type@time, or the token as a header.
    let mut signed = body.clone();
    signed["event"]["hash"] = event_hash(WEBHOOK_ID, "document_viewed", 1_760_000_000.0).into();
    setup
        .server
        .post("/v1/webhooks/signwell")
        .json(&signed)
        .await
        .assert_status_ok();
    assert_eq!(status_of(&pool, id).await, "viewed");
    setup
        .server
        .post("/v1/webhooks/signwell")
        .add_header("x-webhook-token", TOKEN)
        .json(&event("document_declined", "doc-5"))
        .await
        .assert_status_ok();
    assert_eq!(status_of(&pool, id).await, "declined");

    // Not an event, no document, or one we never sent.
    let res = setup
        .server
        .post("/v1/webhooks/signwell")
        .add_query_param("token", TOKEN)
        .json(&json!({ "event": "x" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    let res = setup
        .server
        .post("/v1/webhooks/signwell")
        .add_query_param("token", TOKEN)
        .json(&json!({ "event": { "type": "document_viewed" }, "data": { "object": {} } }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "Missing document id");
    setup
        .send("document_viewed", "unknown")
        .await
        .assert_status_ok();
    setup
        .server
        .get("/v1/webhooks/signwell")
        .await
        .assert_status_ok();
}
