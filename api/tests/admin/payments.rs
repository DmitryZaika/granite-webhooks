//! The payments page's routes (`routes/payments.rs`) against a fake Stripe.
//! Seed: company 100 Acme Stone, 101 Other Granite Co; Ada Admin (102) is
//! made a superuser where a test needs one.

use crate::common;

use api::app;
use api::billing::stripe::Stripe;
use api::routes::payments::{PRODUCT_ID, Payments};
use api::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const LINK_SECRET: &str = "payments-link-test-secret";
const APP_URL: &str = "https://payments.example";

/// A stand-in for the Stripe API: what it holds and every request seen.
#[derive(Default)]
struct FakeStripe {
    /// Subscriptions by customer, newest first.
    subscriptions: HashMap<String, Vec<Value>>,
    /// Invoices by customer.
    invoices: HashMap<String, Vec<Value>>,
    /// Customers whose reads fail.
    failing: Vec<String>,
    /// Products by id.
    products: HashMap<String, Value>,
    portal_configurations: Vec<Value>,
    customers_created: usize,
    /// (method, path, form or query)
    requests: Vec<(String, String, String)>,
}

impl FakeStripe {
    fn count(&self, method: &str, path: &str) -> usize {
        self.requests
            .iter()
            .filter(|(m, p, _)| m == method && p == path)
            .count()
    }

    fn last_form(&self, method: &str, path: &str) -> HashMap<String, String> {
        let (_, _, body) = self
            .requests
            .iter()
            .rev()
            .find(|(m, p, _)| m == method && p == path)
            .unwrap_or_else(|| panic!("no {method} {path}"));
        form(body)
    }
}

type Shared = Arc<Mutex<FakeStripe>>;

fn form(body: &str) -> HashMap<String, String> {
    serde_html_form::from_str(body).unwrap()
}

fn stripe_error(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": { "message": message } })))
}

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn list_for(
    s: &Mutex<FakeStripe>,
    path: &str,
    query: Option<String>,
    pick: fn(&FakeStripe) -> &HashMap<String, Vec<Value>>,
) -> Reply {
    let mut s = s.lock().unwrap();
    let query = query.unwrap_or_default();
    s.requests.push(("GET".into(), path.into(), query.clone()));
    let customer = form(&query).remove("customer").unwrap_or_default();
    if s.failing.contains(&customer) {
        return Err(stripe_error(StatusCode::BAD_REQUEST, "No such customer"));
    }
    let data = pick(&s).get(&customer).cloned().unwrap_or_default();
    Ok(Json(json!({ "object": "list", "data": data })))
}

async fn fake_stripe(state: Shared) -> String {
    let router = Router::new()
        .route(
            "/v1/subscriptions",
            get(
                |State(s): State<Shared>, query: axum::extract::RawQuery| async move {
                    list_for(&s, "/v1/subscriptions", query.0, |s| &s.subscriptions)
                },
            ),
        )
        .route(
            "/v1/invoices",
            get(
                |State(s): State<Shared>, query: axum::extract::RawQuery| async move {
                    list_for(&s, "/v1/invoices", query.0, |s| &s.invoices)
                },
            ),
        )
        .route(
            "/v1/customers",
            post(|State(s): State<Shared>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("POST".into(), "/v1/customers".into(), body));
                s.customers_created += 1;
                Json(json!({ "id": format!("cus_new{}", s.customers_created), "object": "customer" }))
            }),
        )
        .route(
            "/v1/products",
            post(|State(s): State<Shared>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("POST".into(), "/v1/products".into(), body.clone()));
                let params = form(&body);
                let product = json!({ "id": params["id"], "object": "product",
                                      "tax_code": params.get("tax_code") });
                s.products.insert(params["id"].clone(), product.clone());
                Json(product)
            }),
        )
        .route(
            "/v1/products/{id}",
            get(|State(s): State<Shared>, Path(id): Path<String>| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("GET".into(), format!("/v1/products/{id}"), String::new()));
                s.products
                    .get(&id)
                    .cloned()
                    .map(Json)
                    .ok_or_else(|| stripe_error(StatusCode::NOT_FOUND, "No such product"))
            })
            .post(|State(s): State<Shared>, Path(id): Path<String>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("POST".into(), format!("/v1/products/{id}"), body.clone()));
                let tax_code = form(&body).remove("tax_code");
                let product = s
                    .products
                    .get_mut(&id)
                    .ok_or_else(|| stripe_error(StatusCode::NOT_FOUND, "No such product"))?;
                product["tax_code"] = json!(tax_code);
                Ok::<_, (StatusCode, Json<Value>)>(Json(product.clone()))
            }),
        )
        .route(
            "/v1/checkout/sessions",
            post(|State(s): State<Shared>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("POST".into(), "/v1/checkout/sessions".into(), body));
                Json(json!({ "id": "cs_1", "url": "https://checkout.stripe.com/c/pay/cs_1" }))
            }),
        )
        .route(
            "/v1/billing_portal/configurations",
            get(
                |State(s): State<Shared>, query: axum::extract::RawQuery| async move {
                    let mut s = s.lock().unwrap();
                    let query = query.0.unwrap_or_default();
                    s.requests.push((
                        "GET".into(),
                        "/v1/billing_portal/configurations".into(),
                        query.clone(),
                    ));
                    let defaults = form(&query).get("is_default").is_some();
                    let data: Vec<Value> = s
                        .portal_configurations
                        .iter()
                        .filter(|config| !defaults || config["is_default"] == true)
                        .cloned()
                        .collect();
                    Json(json!({ "object": "list", "data": data }))
                },
            )
            .post(|State(s): State<Shared>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push((
                    "POST".into(),
                    "/v1/billing_portal/configurations".into(),
                    body.clone(),
                ));
                let params = form(&body);
                let config = json!({
                    "id": format!("bpc_{}", s.portal_configurations.len() + 1),
                    "is_default": false, "active": true,
                    "metadata": { "granite": params["metadata[granite]"] },
                });
                s.portal_configurations.push(config.clone());
                Json(config)
            }),
        )
        .route(
            "/v1/billing_portal/sessions",
            post(|State(s): State<Shared>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("POST".into(), "/v1/billing_portal/sessions".into(), body));
                Json(json!({ "id": "bps_1", "url": "https://billing.stripe.com/p/session/1" }))
            }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

fn payments() -> Payments {
    Payments::new(LINK_SECRET, APP_URL)
}

fn token(company_id: i32) -> String {
    payments().token(company_id).unwrap()
}

/// The api with payments set up against a fake Stripe.
async fn server_with(pool: &MySqlPool, origins: &str) -> (TestServer, Shared) {
    let shared = Shared::default();
    let stripe = Stripe::new(Some("sk_test_fake"), &fake_stripe(Arc::clone(&shared)).await);
    let state = AppState::new(pool.clone(), SECRET, origins)
        .with_stripe(stripe)
        .with_extension(payments());
    (TestServer::new(app(state)), shared)
}

async fn server(pool: &MySqlPool) -> (TestServer, Shared) {
    server_with(pool, ALLOWED_ORIGIN).await
}

async fn make_superuser(pool: &MySqlPool) {
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(pool)
        .await
        .unwrap();
}

async fn set_customer(pool: &MySqlPool, company_id: i32, customer: &str) {
    sqlx::query("UPDATE company SET stripe_customer_id = ? WHERE id = ?")
        .bind(customer)
        .bind(company_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn stored_customer(pool: &MySqlPool, company_id: i32) -> Option<String> {
    sqlx::query_scalar("SELECT stripe_customer_id FROM company WHERE id = ?")
        .bind(company_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// 2027-01-15T08:00:00Z
const PERIOD_END: i64 = 1_800_000_000;

/// How the api writes a Unix time.
fn iso(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn subscription(id: &str, status: &str, cancel_at_period_end: bool, cents: i64) -> Value {
    json!({
        "id": id, "object": "subscription", "status": status,
        "cancel_at_period_end": cancel_at_period_end, "cancel_at": null,
        "items": { "data": [{ "id": "si_1", "quantity": 1, "current_period_end": PERIOD_END,
                              "price": { "unit_amount": cents, "recurring": { "interval": "month" } } }] },
    })
}

fn paid_invoice(paid_at: i64, cents: i64) -> Value {
    json!({ "id": format!("in_{paid_at}"), "status": "paid", "amount_paid": cents,
            "status_transitions": { "paid_at": paid_at }, "created": paid_at })
}

// ---- configuration and links

#[test_db::test(seed)]
async fn without_stripe_or_a_link_secret_payments_are_off(pool: MySqlPool) {
    let no_stripe = TestServer::new(app(
        AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN).with_extension(payments()),
    ));
    let response = no_stripe.get(&format!("/v1/public/payments/{}", token(100))).await;
    response.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.json::<Value>(), json!({ "error": "Payments are not set up" }));

    let no_secret = TestServer::new(app(
        AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
            .with_stripe(Stripe::new(Some("sk_test_fake"), "http://127.0.0.1:9"))
            .with_extension(Payments::new("", APP_URL)),
    ));
    no_secret
        .post(&format!("/v1/public/payments/{}/checkout", token(100)))
        .await
        .assert_status(StatusCode::SERVICE_UNAVAILABLE);
    make_superuser(&pool).await;
    no_secret
        .get("/v1/superuser/payments")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::SERVICE_UNAVAILABLE);
}

#[test_db::test(seed)]
async fn a_forged_or_stale_link_is_not_found(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    let signature = token(100).split_once('.').unwrap().1.to_string();
    let other_secret = Payments::new("another-secret", APP_URL).token(100).unwrap();
    for bad in [
        format!("101.{signature}"),
        other_secret,
        "100".to_string(),
        "100.short".to_string(),
        "999.abc".to_string(),
    ] {
        for (method, path) in [
            ("GET", format!("/v1/public/payments/{bad}")),
            ("POST", format!("/v1/public/payments/{bad}/checkout")),
            ("POST", format!("/v1/public/payments/{bad}/portal")),
        ] {
            let response = if method == "GET" {
                server.get(&path).await
            } else {
                server.post(&path).await
            };
            response.assert_status(StatusCode::NOT_FOUND);
        }
    }
    // A deleted company's link stops working.
    sqlx::query("UPDATE company SET deleted_at = NOW() WHERE id = 101")
        .execute(&pool)
        .await
        .unwrap();
    server
        .get(&format!("/v1/public/payments/{}", token(101)))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert!(stripe.lock().unwrap().requests.is_empty());
}

// ---- the page

#[test_db::test(seed)]
async fn a_new_company_sees_its_name_and_the_default_price(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    let response = server.get(&format!("/v1/public/payments/{}", token(100))).await;
    response.assert_status_ok();
    assert_eq!(
        response.json::<Value>(),
        json!({ "company_name": "Acme Stone", "price_cents": 80000, "subscription": null })
    );
    // No customer yet, so Stripe is not asked.
    assert!(stripe.lock().unwrap().requests.is_empty());
}

#[test_db::test(seed)]
async fn a_subscribed_company_sees_its_subscription(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    set_customer(&pool, 100, "cus_acme").await;
    stripe.lock().unwrap().subscriptions.insert(
        "cus_acme".into(),
        vec![
            subscription("sub_new", "active", false, 65000),
            subscription("sub_old", "canceled", false, 80000),
        ],
    );
    let page: Value = server
        .get(&format!("/v1/public/payments/{}", token(100)))
        .await
        .json();
    assert_eq!(
        page["subscription"],
        json!({
            "id": "sub_new", "status": "active", "price_cents": 65000, "renews": true,
            "next_payment_at": "2027-01-15T08:00:00Z", "ends_at": null,
        })
    );

    // Set to cancel: still the company's until it ends.
    stripe.lock().unwrap().subscriptions.insert(
        "cus_acme".into(),
        vec![subscription("sub_new", "active", true, 65000)],
    );
    let page: Value = server
        .get(&format!("/v1/public/payments/{}", token(100)))
        .await
        .json();
    assert_eq!(page["subscription"]["renews"], false);
    assert_eq!(page["subscription"]["ends_at"], "2027-01-15T08:00:00Z");

    // Only ended subscriptions: the company can subscribe again.
    stripe.lock().unwrap().subscriptions.insert(
        "cus_acme".into(),
        vec![subscription("sub_new", "canceled", false, 65000)],
    );
    let page: Value = server
        .get(&format!("/v1/public/payments/{}", token(100)))
        .await
        .json();
    assert_eq!(page["subscription"], Value::Null);
}

#[test_db::test(seed)]
async fn stripe_failing_is_a_bad_gateway(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    set_customer(&pool, 100, "cus_gone").await;
    stripe.lock().unwrap().failing.push("cus_gone".into());
    let response = server
        .get(&format!("/v1/public/payments/{}", token(100)))
        .await;
    response.assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.json::<Value>(),
        json!({ "error": "Stripe: No such customer" })
    );
}

// ---- checkout

#[test_db::test(seed)]
async fn the_first_checkout_links_a_new_customer_and_charges_the_company_price(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    let response = server
        .post(&format!("/v1/public/payments/{}/checkout", token(100)))
        .await;
    response.assert_status_ok();
    assert_eq!(
        response.json::<Value>(),
        json!({ "url": "https://checkout.stripe.com/c/pay/cs_1" })
    );
    // The customer is saved on the company before the payer pays.
    assert_eq!(stored_customer(&pool, 100).await.as_deref(), Some("cus_new1"));
    let s = stripe.lock().unwrap();
    let customer = s.last_form("POST", "/v1/customers");
    assert_eq!(customer["name"], "Acme Stone");
    assert_eq!(customer["metadata[company_id]"], "100");
    assert_eq!(
        s.last_form("POST", "/v1/products"),
        HashMap::from([
            ("id".to_string(), PRODUCT_ID.to_string()),
            ("name".to_string(), "Granite Manager subscription".to_string()),
            ("tax_code".to_string(), "txcd_10103001".to_string()),
        ])
    );
    let session = s.last_form("POST", "/v1/checkout/sessions");
    let page = format!("{APP_URL}/?c={}", token(100));
    assert_eq!(
        session,
        HashMap::from(
            [
                ("mode", "subscription"),
                ("customer", "cus_new1"),
                ("client_reference_id", "100"),
                ("line_items[0][quantity]", "1"),
                ("line_items[0][price_data][currency]", "usd"),
                ("line_items[0][price_data][product]", PRODUCT_ID),
                ("line_items[0][price_data][unit_amount]", "80000"),
                ("line_items[0][price_data][recurring][interval]", "month"),
                ("success_url", &format!("{page}&checkout=success")),
                ("cancel_url", &format!("{page}&checkout=cancel")),
                ("metadata[company_id]", "100"),
                ("subscription_data[metadata][company_id]", "100"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string()))
        )
    );
}

#[test_db::test(seed)]
async fn a_later_checkout_reuses_the_customer_and_product_at_the_current_price(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    for _ in 0..2 {
        server
            .post(&format!("/v1/public/payments/{}/checkout", token(100)))
            .await
            .assert_status_ok();
    }
    make_superuser(&pool).await;
    server
        .put("/v1/superuser/companies/100/subscription-price")
        .with_session(sessions::ADMIN)
        .json(&json!({ "price_cents": 123_456 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .post(&format!("/v1/public/payments/{}/checkout", token(100)))
        .await
        .assert_status_ok();
    let s = stripe.lock().unwrap();
    assert_eq!(s.count("POST", "/v1/customers"), 1);
    assert_eq!(s.count("POST", "/v1/products"), 1);
    assert_eq!(s.count("POST", "/v1/checkout/sessions"), 3);
    let session = s.last_form("POST", "/v1/checkout/sessions");
    assert_eq!(session["customer"], "cus_new1");
    assert_eq!(session["line_items[0][price_data][unit_amount]"], "123456");
}

#[test_db::test(seed)]
async fn a_product_made_without_a_tax_code_gets_one(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    stripe.lock().unwrap().products.insert(
        PRODUCT_ID.into(),
        json!({ "id": PRODUCT_ID, "object": "product", "tax_code": null }),
    );
    for _ in 0..2 {
        server
            .post(&format!("/v1/public/payments/{}/checkout", token(100)))
            .await
            .assert_status_ok();
    }
    let s = stripe.lock().unwrap();
    assert_eq!(s.count("POST", "/v1/products"), 0);
    let path = format!("/v1/products/{PRODUCT_ID}");
    assert_eq!(s.count("POST", &path), 1);
    assert_eq!(s.last_form("POST", &path)["tax_code"], "txcd_10103001");
}

#[test_db::test(seed)]
async fn a_company_with_a_live_subscription_cannot_check_out_again(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    set_customer(&pool, 100, "cus_acme").await;
    for status in ["active", "trialing", "past_due", "unpaid", "paused", "incomplete"] {
        stripe.lock().unwrap().subscriptions.insert(
            "cus_acme".into(),
            vec![subscription("sub_1", status, status == "active", 80000)],
        );
        let response = server
            .post(&format!("/v1/public/payments/{}/checkout", token(100)))
            .await;
        response.assert_status(StatusCode::CONFLICT);
        assert_eq!(
            response.json::<Value>(),
            json!({ "error": "You already have a subscription" }),
            "{status}"
        );
    }
    assert_eq!(stripe.lock().unwrap().count("POST", "/v1/checkout/sessions"), 0);
    // After it ended, a new one can start (same customer).
    stripe.lock().unwrap().subscriptions.insert(
        "cus_acme".into(),
        vec![subscription("sub_1", "canceled", false, 80000)],
    );
    server
        .post(&format!("/v1/public/payments/{}/checkout", token(100)))
        .await
        .assert_status_ok();
    let s = stripe.lock().unwrap();
    assert_eq!(s.count("POST", "/v1/customers"), 0);
    assert_eq!(s.last_form("POST", "/v1/checkout/sessions")["customer"], "cus_acme");
}

#[test_db::test(seed)]
async fn checking_out_never_changes_access_to_the_crm(pool: MySqlPool) {
    let (server, _stripe) = server(&pool).await;
    server
        .post(&format!("/v1/public/payments/{}/checkout", token(100)))
        .await
        .assert_status_ok();
    let required: i8 = sqlx::query_scalar("SELECT billing_required FROM company WHERE id = 100")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(required, 0);
    let billing_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM company_billing")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(billing_rows, 0);
}

#[test_db::test(seed)]
async fn the_payments_page_origin_may_post(pool: MySqlPool) {
    let (server, _stripe) =
        server_with(&pool, "https://granite-manager.com,https://*.granite-manager.com").await;
    for origin in [
        "https://payments.granite-manager.com",
        "https://dev-payments.granite-manager.com",
    ] {
        server
            .post(&format!("/v1/public/payments/{}/checkout", token(100)))
            .add_header("origin", origin)
            .await
            .assert_status_ok();
    }
    server
        .post(&format!("/v1/public/payments/{}/checkout", token(100)))
        .add_header("origin", "https://evil.example")
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

// ---- portal

#[test_db::test(seed)]
async fn the_portal_needs_a_customer(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    let response = server
        .post(&format!("/v1/public/payments/{}/portal", token(100)))
        .await;
    response.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>(),
        json!({ "error": "There is no subscription to manage yet" })
    );
    assert!(stripe.lock().unwrap().requests.is_empty());
}

#[test_db::test(seed)]
async fn the_portal_makes_its_configuration_once_and_returns_to_the_page(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    set_customer(&pool, 100, "cus_acme").await;
    for _ in 0..2 {
        let response = server
            .post(&format!("/v1/public/payments/{}/portal", token(100)))
            .await;
        response.assert_status_ok();
        assert_eq!(
            response.json::<Value>(),
            json!({ "url": "https://billing.stripe.com/p/session/1" })
        );
    }
    let s = stripe.lock().unwrap();
    assert_eq!(s.count("POST", "/v1/billing_portal/configurations"), 1);
    let config = s.last_form("POST", "/v1/billing_portal/configurations");
    assert_eq!(config["features[subscription_cancel][enabled]"], "true");
    assert_eq!(config["features[subscription_cancel][mode]"], "at_period_end");
    assert_eq!(config["features[payment_method_update][enabled]"], "true");
    assert_eq!(config["features[invoice_history][enabled]"], "true");
    let session = s.last_form("POST", "/v1/billing_portal/sessions");
    assert_eq!(
        session,
        HashMap::from(
            [
                ("customer", "cus_acme".to_string()),
                ("return_url", format!("{APP_URL}/?c={}", token(100))),
                ("configuration", "bpc_1".to_string()),
            ]
            .map(|(k, v)| (k.to_string(), v))
        )
    );
}

#[test_db::test(seed)]
async fn the_portal_uses_the_account_default_configuration_when_there_is_one(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    set_customer(&pool, 100, "cus_acme").await;
    stripe
        .lock()
        .unwrap()
        .portal_configurations
        .push(json!({ "id": "bpc_default", "is_default": true, "active": true, "metadata": {} }));
    server
        .post(&format!("/v1/public/payments/{}/portal", token(100)))
        .await
        .assert_status_ok();
    let s = stripe.lock().unwrap();
    assert_eq!(s.count("POST", "/v1/billing_portal/configurations"), 0);
    assert!(!s
        .last_form("POST", "/v1/billing_portal/sessions")
        .contains_key("configuration"));
}

// ---- superuser screen

#[test_db::test(seed)]
async fn only_superusers_see_and_price_the_payments(pool: MySqlPool) {
    let (server, _stripe) = server(&pool).await;
    server
        .get("/v1/superuser/payments")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    for session in [sessions::REP, sessions::MANAGER, sessions::ADMIN] {
        server
            .get("/v1/superuser/payments")
            .with_session(session)
            .await
            .assert_status(StatusCode::FORBIDDEN);
        server
            .put("/v1/superuser/companies/100/subscription-price")
            .with_session(session)
            .json(&json!({ "price_cents": 100 }))
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
}

#[test_db::test(seed)]
async fn superusers_see_every_company_s_payments(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    make_superuser(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO company (id, name) VALUES (102, 'Bravo Marble'), (103, 'Gone Granite');
         UPDATE company SET deleted_at = NOW() WHERE id = 103;
         UPDATE company SET subscription_price_cents = 50000 WHERE id = 101;",
    )
    .execute(&pool)
    .await
    .unwrap();
    set_customer(&pool, 100, "cus_acme").await;
    set_customer(&pool, 101, "cus_other").await;
    set_customer(&pool, 102, "cus_broken").await;
    {
        let mut s = stripe.lock().unwrap();
        s.subscriptions.insert(
            "cus_acme".into(),
            vec![subscription("sub_acme", "active", false, 80000)],
        );
        s.invoices.insert(
            "cus_acme".into(),
            vec![paid_invoice(1_797_000_000, 80000), paid_invoice(1_794_000_000, 80000)],
        );
        s.subscriptions.insert(
            "cus_other".into(),
            vec![subscription("sub_other", "canceled", false, 50000)],
        );
        s.invoices
            .insert("cus_other".into(), vec![paid_invoice(1_790_000_000, 50000)]);
        s.failing.push("cus_broken".into());
    }
    let response = server
        .get("/v1/superuser/payments")
        .with_session(sessions::ADMIN)
        .await;
    response.assert_status_ok();
    assert_eq!(
        response.json::<Value>(),
        json!([
            {
                "id": 100, "name": "Acme Stone", "subscription_price_cents": 80000,
                "payment_link": format!("{APP_URL}/?c={}", token(100)),
                "stripe_customer_id": "cus_acme",
                "subscription": {
                    "id": "sub_acme", "status": "active", "price_cents": 80000, "renews": true,
                    "next_payment_at": "2027-01-15T08:00:00Z", "ends_at": null,
                },
                "last_payment_at": iso(1_797_000_000), "last_payment_cents": 80000,
                "stripe_error": null,
            },
            {
                "id": 102, "name": "Bravo Marble", "subscription_price_cents": 80000,
                "payment_link": format!("{APP_URL}/?c={}", token(102)),
                "stripe_customer_id": "cus_broken", "subscription": null,
                "last_payment_at": null, "last_payment_cents": null,
                "stripe_error": "No such customer",
            },
            {
                // Made by the migrations; never checked out.
                "id": 1, "name": "Default Company", "subscription_price_cents": 80000,
                "payment_link": format!("{APP_URL}/?c={}", token(1)),
                "stripe_customer_id": null, "subscription": null,
                "last_payment_at": null, "last_payment_cents": null, "stripe_error": null,
            },
            {
                "id": 101, "name": "Other Granite Co", "subscription_price_cents": 50000,
                "payment_link": format!("{APP_URL}/?c={}", token(101)),
                "stripe_customer_id": "cus_other",
                "subscription": {
                    "id": "sub_other", "status": "canceled", "price_cents": 50000, "renews": false,
                    "next_payment_at": null, "ends_at": null,
                },
                "last_payment_at": iso(1_790_000_000), "last_payment_cents": 50000,
                "stripe_error": null,
            },
        ])
    );
    // A link from the list opens that company's page.
    let link = response.json::<Value>()[3]["payment_link"].as_str().unwrap().to_string();
    let token = link.split("?c=").nth(1).unwrap();
    let page: Value = server.get(&format!("/v1/public/payments/{token}")).await.json();
    assert_eq!(page["company_name"], "Other Granite Co");
    assert_eq!(page["price_cents"], 50000);
}

#[test_db::test(seed)]
async fn superusers_set_a_company_price(pool: MySqlPool) {
    let (server, _stripe) = server(&pool).await;
    make_superuser(&pool).await;
    for bad in [0, 49, -1, 10_000_001] {
        let response = server
            .put("/v1/superuser/companies/100/subscription-price")
            .with_session(sessions::ADMIN)
            .json(&json!({ "price_cents": bad }))
            .await;
        response.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json::<Value>(),
            json!({ "error": "The price must be between $0.50 and $100,000" })
        );
    }
    server
        .put("/v1/superuser/companies/999/subscription-price")
        .with_session(sessions::ADMIN)
        .json(&json!({ "price_cents": 1000 }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // The same price again is still a success.
    for _ in 0..2 {
        server
            .put("/v1/superuser/companies/101/subscription-price")
            .with_session(sessions::ADMIN)
            .json(&json!({ "price_cents": 95000 }))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    let prices: Vec<(i32, i32)> =
        sqlx::query_as("SELECT id, subscription_price_cents FROM company ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(prices, vec![(1, 80000), (100, 80000), (101, 95000)]);
}
