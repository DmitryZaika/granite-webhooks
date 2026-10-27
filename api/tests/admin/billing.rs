use crate::common;

use api::app;
use api::billing::stripe::{Stripe, signature_header};
use api::billing::{BillingPlan, parse_billing_plans};
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
use std::time::Duration;

#[path = "billing/account.rs"]
mod account;
#[path = "billing/events.rs"]
mod events;
#[path = "billing/gate.rs"]
mod gate;

const WEBHOOK_SECRET: &str = "whsec_test_secret";

/// A stand-in for the Stripe API: objects by id and every request seen.
#[derive(Default)]
struct FakeStripe {
    subscriptions: HashMap<String, Value>,
    invoices: HashMap<String, Value>,
    payment_methods: HashMap<String, Value>,
    prices: HashMap<String, Value>,
    customers: HashMap<String, Value>,
    portal_configurations: Vec<Value>,
    /// (method, path, form or query)
    requests: Vec<(String, String, String)>,
}

type Shared = Arc<Mutex<FakeStripe>>;

fn not_found() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": { "message": "No such object" } })),
    )
}

fn form(body: &str) -> HashMap<String, String> {
    serde_html_form::from_str(body).unwrap()
}

async fn fake_stripe(state: Shared) -> String {
    let router = Router::new()
        .route(
            "/v1/subscriptions",
            get(|State(s): State<Shared>, query: axum::extract::RawQuery| async move {
                let mut s = s.lock().unwrap();
                let query = query.0.unwrap_or_default();
                s.requests.push(("GET".into(), "/v1/subscriptions".into(), query.clone()));
                let customer = form(&query).remove("customer").unwrap_or_default();
                let data: Vec<Value> = s
                    .subscriptions
                    .values()
                    .filter(|sub| sub["customer"] == customer.as_str())
                    .cloned()
                    .collect();
                Json(json!({ "object": "list", "data": data }))
            })
            .post(|State(s): State<Shared>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("POST".into(), "/v1/subscriptions".into(), body.clone()));
                let params = form(&body);
                let sub = json!({
                    "id": "sub_new", "object": "subscription", "status": "incomplete",
                    "customer": params["customer"],
                    "default_payment_method": params["default_payment_method"],
                    "metadata": { "company_id": params["metadata[company_id]"] },
                    "items": { "data": [{ "id": "si_new", "quantity": 1,
                        "price": { "id": params["items[0][price]"], "nickname": "Granite Manager", "unit_amount": 4900 } }] },
                    "latest_invoice": { "id": "in_new", "status": "open",
                        "confirmation_secret": { "client_secret": "pi_new_secret_abc" } },
                });
                s.subscriptions.insert("sub_new".into(), sub.clone());
                Json(sub)
            }),
        )
        .route(
            "/v1/subscriptions/{id}",
            get(|State(s): State<Shared>, Path(id): Path<String>| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("GET".into(), format!("/v1/subscriptions/{id}"), String::new()));
                s.subscriptions.get(&id).cloned().map(Json).ok_or_else(not_found)
            })
            .post(|State(s): State<Shared>, Path(id): Path<String>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("POST".into(), format!("/v1/subscriptions/{id}"), body.clone()));
                let params = form(&body);
                let sub = s.subscriptions.get_mut(&id).ok_or_else(not_found)?;
                if let Some(price) = params.get("items[0][price]") {
                    sub["items"]["data"][0]["price"] = json!({
                        "id": price, "nickname": "Granite Manager Plus", "unit_amount": 9900 });
                }
                if let Some(method) = params.get("default_payment_method") {
                    sub["default_payment_method"] = json!(method);
                }
                Ok::<_, (StatusCode, Json<Value>)>(Json(sub.clone()))
            })
            .delete(|State(s): State<Shared>, Path(id): Path<String>| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("DELETE".into(), format!("/v1/subscriptions/{id}"), String::new()));
                let sub = s.subscriptions.get_mut(&id).ok_or_else(not_found)?;
                sub["status"] = json!("canceled");
                Ok::<_, (StatusCode, Json<Value>)>(Json(sub.clone()))
            }),
        )
        .route(
            "/v1/invoices/{id}",
            get(|State(s): State<Shared>, Path(id): Path<String>, query: axum::extract::RawQuery| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("GET".into(), format!("/v1/invoices/{id}"), query.0.unwrap_or_default()));
                s.invoices.get(&id).cloned().map(Json).ok_or_else(not_found)
            }),
        )
        .route(
            "/v1/invoices",
            get(|State(s): State<Shared>, query: axum::extract::RawQuery| async move {
                let mut s = s.lock().unwrap();
                let query = query.0.unwrap_or_default();
                s.requests.push(("GET".into(), "/v1/invoices".into(), query.clone()));
                let params = form(&query);
                let mut data: Vec<Value> = s
                    .invoices
                    .values()
                    .filter(|invoice| {
                        params.get("customer").is_none_or(|c| invoice["customer"] == c.as_str())
                            && params.get("status").is_none_or(|st| invoice["status"] == st.as_str())
                    })
                    .cloned()
                    .collect();
                // Newest first, like Stripe.
                data.sort_by_key(|invoice| {
                    std::cmp::Reverse((invoice["created"].as_i64(), invoice["id"].as_str().map(str::to_string)))
                });
                Json(json!({ "object": "list", "data": data }))
            }),
        )
        .route(
            "/v1/customers",
            post(|State(s): State<Shared>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("POST".into(), "/v1/customers".into(), body));
                Json(json!({ "id": "cus_created", "object": "customer" }))
            }),
        )
        .route(
            "/v1/customers/{id}/payment_methods",
            get(|State(s): State<Shared>, Path(id): Path<String>| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("GET".into(), format!("/v1/customers/{id}/payment_methods"), String::new()));
                let data: Vec<Value> = s
                    .payment_methods
                    .values()
                    .filter(|m| m["customer"] == id.as_str())
                    .cloned()
                    .collect();
                Json(json!({ "object": "list", "data": data }))
            }),
        )
        .route(
            "/v1/checkout/sessions",
            post(|State(s): State<Shared>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("POST".into(), "/v1/checkout/sessions".into(), body));
                Json(json!({ "id": "cs_1", "url": "https://checkout.stripe.com/c/cs_1" }))
            }),
        )
        .route(
            "/v1/billing_portal/configurations",
            get(|State(s): State<Shared>, query: axum::extract::RawQuery| async move {
                let mut s = s.lock().unwrap();
                let query = query.0.unwrap_or_default();
                s.requests.push(("GET".into(), "/v1/billing_portal/configurations".into(), query));
                Json(json!({ "object": "list", "data": s.portal_configurations }))
            })
            .post(|State(s): State<Shared>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("POST".into(), "/v1/billing_portal/configurations".into(), body.clone()));
                let configuration = json!({ "id": "bpc_created", "active": true,
                    "metadata": { "app": form(&body)["metadata[app]"] } });
                s.portal_configurations.push(configuration.clone());
                Json(configuration)
            }),
        )
        .route(
            "/v1/prices/{id}",
            get(|State(s): State<Shared>, Path(id): Path<String>| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("GET".into(), format!("/v1/prices/{id}"), String::new()));
                s.prices.get(&id).cloned().map(Json).ok_or_else(not_found)
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
        .route(
            "/v1/payment_methods/{id}",
            get(|State(s): State<Shared>, Path(id): Path<String>| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("GET".into(), format!("/v1/payment_methods/{id}"), String::new()));
                s.payment_methods.get(&id).cloned().map(Json).ok_or_else(not_found)
            }),
        )
        .route(
            "/v1/payment_methods/{id}/attach",
            post(|State(s): State<Shared>, Path(id): Path<String>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("POST".into(), format!("/v1/payment_methods/{id}/attach"), body.clone()));
                let customer = form(&body).remove("customer").unwrap_or_default();
                let method = s.payment_methods.get_mut(&id).ok_or_else(not_found)?;
                method["customer"] = json!(customer);
                Ok::<_, (StatusCode, Json<Value>)>(Json(method.clone()))
            }),
        )
        .route(
            "/v1/customers/{id}",
            get(|State(s): State<Shared>, Path(id): Path<String>, query: axum::extract::RawQuery| async move {
                let mut s = s.lock().unwrap();
                let query = query.0.unwrap_or_default();
                s.requests.push(("GET".into(), format!("/v1/customers/{id}"), query.clone()));
                let mut customer = s.customers.get(&id).cloned().ok_or_else(not_found)?;
                let default = &customer["invoice_settings"]["default_payment_method"];
                if form(&query).get("expand[]").map(String::as_str)
                    == Some("invoice_settings.default_payment_method")
                    && let Some(method) = default.as_str().and_then(|id| s.payment_methods.get(id))
                {
                    customer["invoice_settings"]["default_payment_method"] = method.clone();
                }
                Ok::<_, (StatusCode, Json<Value>)>(Json(customer))
            })
            .post(|State(s): State<Shared>, Path(id): Path<String>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("POST".into(), format!("/v1/customers/{id}"), body));
                Json(json!({ "id": id, "object": "customer" }))
            }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

fn plans() -> Vec<BillingPlan> {
    parse_billing_plans(
        Some("price_flat|Granite Manager|4900,price_plus|Granite Manager Plus|9900"),
        None,
    )
}

/// The api with the webhook secret and, when given, a fake Stripe.
async fn server(pool: &MySqlPool, stripe: Option<&Shared>) -> TestServer {
    let mut state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_stripe_webhook_secret(WEBHOOK_SECRET)
        .with_billing_plans(plans());
    if let Some(shared) = stripe {
        let mut client = Stripe::new(Some("sk_test_fake"), &fake_stripe(Arc::clone(shared)).await);
        client.poll_delay = Duration::from_millis(1);
        state = state.with_stripe(client);
    }
    TestServer::new(app(state))
}

/// Mark (101) becomes a Manager of company 100, which has billing switched on.
async fn manager_and_billing(pool: &MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 5, 100);
         UPDATE company SET billing_required = 1 WHERE id = 100;",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn post_event(server: &TestServer, event: &Value) -> axum_test::TestResponse {
    let payload = serde_json::to_vec(event).unwrap();
    let header = signature_header(&payload, WEBHOOK_SECRET, chrono::Utc::now().timestamp());
    server
        .post("/v1/webhooks/stripe")
        .add_header("stripe-signature", header)
        .add_header("content-type", "application/json")
        .bytes(payload.into())
        .await
}

fn invoice(id: &str, kind_paid: bool, amount: i64) -> Value {
    json!({
        "id": id, "object": "invoice", "customer": "cus_100",
        "status": if kind_paid { "paid" } else { "open" },
        "amount_due": amount, "amount_paid": if kind_paid { amount } else { 0 },
        "currency": "usd", "hosted_invoice_url": "https://pay.stripe.com/i", "invoice_pdf": null,
        "status_transitions": { "paid_at": if kind_paid { json!(1_788_220_800) } else { Value::Null } },
        "parent": { "subscription_details": { "subscription": "sub_100", "metadata": { "company_id": "100" } } },
    })
}

fn event(id: &str, kind: &str, object: Value) -> Value {
    json!({ "id": id, "object": "event", "type": kind, "data": { "object": object } })
}

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct Row {
    stripe_customer_id: Option<String>,
    stripe_subscription_id: Option<String>,
    subscription_status: String,
    payment_status: String,
    grace: bool,
    outstanding_balance_cents: i32,
}

async fn billing_row(pool: &MySqlPool, company_id: i32) -> Option<Row> {
    sqlx::query_as(
        "SELECT stripe_customer_id, stripe_subscription_id, subscription_status, payment_status,
                grace_period_ends_at IS NOT NULL AS grace, outstanding_balance_cents
           FROM company_billing WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn open_notifications(pool: &MySqlPool) -> Vec<(i32, String, String)> {
    sqlx::query_as(
        "SELECT user_id, title, href FROM billing_notifications WHERE is_done = 0 ORDER BY user_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

// ---- POST /v1/webhooks/stripe

#[test_db::test(seed)]
async fn the_webhook_needs_a_valid_signature(pool: MySqlPool) {
    let unconfigured = TestServer::new(app(AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)));
    let body =
        serde_json::to_vec(&event("evt_1", "invoice.paid", invoice("in_1", true, 100))).unwrap();
    let header = signature_header(&body, WEBHOOK_SECRET, chrono::Utc::now().timestamp());
    let response = unconfigured
        .post("/v1/webhooks/stripe")
        .add_header("stripe-signature", header.clone())
        .bytes(body.clone().into())
        .await;
    response.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>(),
        json!({ "error": "Webhook is not configured" })
    );

    let server = server(&pool, None).await;
    let missing = server
        .post("/v1/webhooks/stripe")
        .bytes(body.clone().into())
        .await;
    missing.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        missing.json::<Value>(),
        json!({ "error": "Webhook is not configured" })
    );
    let wrong = signature_header(&body, "whsec_other", chrono::Utc::now().timestamp());
    let forged = server
        .post("/v1/webhooks/stripe")
        .add_header("stripe-signature", wrong)
        .bytes(body.clone().into())
        .await;
    forged.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        forged.json::<Value>(),
        json!({ "error": "Invalid webhook signature" })
    );
    let stale = signature_header(&body, WEBHOOK_SECRET, chrono::Utc::now().timestamp() - 3600);
    server
        .post("/v1/webhooks/stripe")
        .add_header("stripe-signature", stale)
        .bytes(body.into())
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(billing_row(&pool, 100).await, None);
}

#[test_db::test(seed)]
async fn a_failed_then_paid_invoice_runs_the_grace_period(pool: MySqlPool) {
    manager_and_billing(&pool).await;
    let server = server(&pool, None).await;

    let failed = event(
        "evt_fail",
        "invoice.payment_failed",
        invoice("in_1", false, 4900),
    );
    let response = post_event(&server, &failed).await;
    response.assert_status_ok();
    assert_eq!(response.json::<Value>(), json!({ "received": true }));
    assert_eq!(
        billing_row(&pool, 100).await,
        Some(Row {
            stripe_customer_id: Some("cus_100".into()),
            stripe_subscription_id: Some("sub_100".into()),
            subscription_status: "past_due".into(),
            payment_status: "past_due".into(),
            grace: true,
            outstanding_balance_cents: 4900,
        })
    );
    assert_eq!(
        open_notifications(&pool).await,
        [(
            101,
            "Payment required".to_string(),
            "/admin/billing".to_string()
        )]
    );
    let message: String = sqlx::query_scalar("SELECT message FROM billing_notifications")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(message.starts_with("Your Granite Manager subscription payment was unsuccessful."));

    // Stripe delivers the same event again: nothing changes.
    post_event(&server, &failed).await.assert_status_ok();
    assert_eq!(open_notifications(&pool).await.len(), 1);
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stripe_webhook_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(events, 1);

    post_event(
        &server,
        &event("evt_paid", "invoice.paid", invoice("in_1", true, 4900)),
    )
    .await
    .assert_status_ok();
    let row = billing_row(&pool, 100).await.unwrap();
    assert_eq!(
        (
            row.payment_status.as_str(),
            row.grace,
            row.outstanding_balance_cents
        ),
        ("active", false, 0)
    );
    assert!(open_notifications(&pool).await.is_empty());
    let invoice: (String, i32, Option<chrono::NaiveDateTime>) = sqlx::query_as(
        "SELECT status, amount_due_cents, paid_at FROM company_billing_invoices WHERE stripe_invoice_id = 'in_1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(invoice.0, "paid");
    assert_eq!(invoice.1, 0);
    assert_eq!(invoice.2.unwrap().to_string(), "2026-09-01 00:00:00");
}

#[test_db::test(seed)]
async fn no_notification_while_billing_is_switched_off(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 5, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(&pool, None).await;
    post_event(
        &server,
        &event(
            "evt_fail",
            "invoice.payment_failed",
            invoice("in_1", false, 4900),
        ),
    )
    .await
    .assert_status_ok();
    assert_eq!(
        billing_row(&pool, 100).await.unwrap().payment_status,
        "past_due"
    );
    assert!(open_notifications(&pool).await.is_empty());
}

#[test_db::test(seed)]
async fn checkout_subscription_and_card_events_fill_the_record(pool: MySqlPool) {
    let server = server(&pool, None).await;
    post_event(
        &server,
        &event(
            "evt_checkout",
            "checkout.session.completed",
            json!({ "object": "checkout.session", "customer": "cus_101", "subscription": "sub_101",
                    "client_reference_id": "101", "metadata": {} }),
        ),
    )
    .await
    .assert_status_ok();
    post_event(
        &server,
        &event(
            "evt_sub",
            "customer.subscription.created",
            json!({ "id": "sub_101", "object": "subscription", "status": "active", "customer": "cus_101",
                    "default_payment_method": "pm_1", "metadata": { "company_id": "101" },
                    "items": { "data": [{ "id": "si_1", "quantity": 2,
                        "current_period_start": 1_788_220_800, "current_period_end": 1_790_812_800,
                        "price": { "id": "price_flat", "nickname": "Granite Manager", "unit_amount": 4900 } }] } }),
        ),
    )
    .await
    .assert_status_ok();
    post_event(
        &server,
        &event(
            "evt_pm",
            "payment_method.updated",
            json!({ "id": "pm_2", "object": "payment_method", "customer": "cus_101" }),
        ),
    )
    .await
    .assert_status_ok();
    // Events billing does not use are acknowledged.
    post_event(
        &server,
        &event(
            "evt_other",
            "charge.succeeded",
            json!({ "object": "charge" }),
        ),
    )
    .await
    .assert_status_ok();
    let row: (String, String, String, i32, i32, String, String) = sqlx::query_as(
        "SELECT stripe_customer_id, stripe_subscription_id, payment_status, monthly_price_cents, user_count,
                stripe_payment_method_id, DATE_FORMAT(current_period_end, '%Y-%m-%d')
           FROM company_billing WHERE company_id = 101",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "cus_101".into(),
            "sub_101".into(),
            "active".into(),
            9800,
            2,
            "pm_2".into(),
            "2026-10-01".into()
        )
    );
    assert_eq!(billing_row(&pool, 100).await, None);
}

#[test_db::test(seed)]
async fn a_late_failure_stripe_has_since_paid_or_voided(pool: MySqlPool) {
    manager_and_billing(&pool).await;
    let stripe = Shared::default();
    stripe
        .lock()
        .unwrap()
        .invoices
        .insert("in_paid".into(), invoice("in_paid", true, 4900));
    let mut voided = invoice("in_void", false, 4900);
    voided["status"] = json!("void");
    stripe
        .lock()
        .unwrap()
        .invoices
        .insert("in_void".into(), voided);
    let server = server(&pool, Some(&stripe)).await;

    post_event(
        &server,
        &event(
            "evt_void",
            "invoice.payment_failed",
            invoice("in_void", false, 4900),
        ),
    )
    .await
    .assert_status_ok();
    assert_eq!(billing_row(&pool, 100).await, None);

    post_event(
        &server,
        &event(
            "evt_late",
            "invoice.payment_failed",
            invoice("in_paid", false, 4900),
        ),
    )
    .await
    .assert_status_ok();
    let row = billing_row(&pool, 100).await.unwrap();
    assert_eq!((row.payment_status.as_str(), row.grace), ("active", false));
    assert!(open_notifications(&pool).await.is_empty());
}

#[test_db::test(seed)]
async fn a_stripe_failure_is_a_500_so_stripe_retries(pool: MySqlPool) {
    let stripe = Shared::default();
    let server = server(&pool, Some(&stripe)).await;
    // The live invoice cannot be read (unknown to the fake Stripe).
    let response = post_event(
        &server,
        &event(
            "evt_x",
            "invoice.payment_failed",
            invoice("in_gone", false, 1),
        ),
    )
    .await;
    response.assert_status(StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response.json::<Value>(),
        json!({ "error": "Webhook processing failed" })
    );
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stripe_webhook_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(events, 0);
}

// ---- POST /v1/billing/reactivation (+ /finish)

/// Company 100 was suspended: its subscription `sub_old` was canceled unpaid.
async fn suspended(pool: &MySqlPool, stripe: &Shared) {
    manager_and_billing(pool).await;
    sqlx::query(
        "INSERT INTO company_billing (company_id, stripe_customer_id, stripe_subscription_id, stripe_price_id,
                                      subscription_status, payment_status, user_count, monthly_price_cents)
         VALUES (100, 'cus_100', 'sub_old', 'price_flat', 'canceled', 'suspended', 3, 14700)",
    )
    .execute(pool)
    .await
    .unwrap();
    let mut fake = stripe.lock().unwrap();
    fake.subscriptions.insert(
        "sub_old".into(),
        json!({ "id": "sub_old", "object": "subscription", "status": "canceled", "customer": "cus_100",
                "metadata": { "company_id": "100" }, "items": { "data": [] } }),
    );
    fake.payment_methods.insert(
        "pm_new".into(),
        json!({ "id": "pm_new", "object": "payment_method", "type": "card", "customer": null }),
    );
    fake.payment_methods.insert(
        "pm_foreign".into(),
        json!({ "id": "pm_foreign", "object": "payment_method", "type": "card", "customer": "cus_other" }),
    );
}

#[test_db::test(seed)]
async fn only_a_manager_may_pay(pool: MySqlPool) {
    let stripe = Shared::default();
    suspended(&pool, &stripe).await;
    let server = server(&pool, Some(&stripe)).await;
    let body = json!({ "payment_method_id": "pm_new" });
    server
        .post("/v1/billing/reactivation")
        .json(&body)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    for session in [sessions::REP, sessions::ADMIN, sessions::OTHER_COMPANY_REP] {
        server
            .post("/v1/billing/reactivation")
            .with_session(session)
            .json(&body)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    server
        .post("/v1/billing/reactivation/finish")
        .with_session(sessions::REP)
        .json(&json!({}))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn a_company_that_is_not_restricted_has_nothing_to_pay(pool: MySqlPool) {
    manager_and_billing(&pool).await;
    let stripe = Shared::default();
    let server = server(&pool, Some(&stripe)).await;
    let started = server
        .post("/v1/billing/reactivation")
        .with_session(sessions::MANAGER)
        .json(&json!({ "payment_method_id": "pm_new" }))
        .await
        .json::<Value>();
    assert_eq!(started, json!({ "status": "paid" }));
    assert!(stripe.lock().unwrap().requests.is_empty());
}

#[test_db::test(seed)]
async fn a_suspended_company_pays_with_a_new_card_and_is_restored(pool: MySqlPool) {
    let stripe = Shared::default();
    suspended(&pool, &stripe).await;
    let server = server(&pool, Some(&stripe)).await;

    let no_card = server
        .post("/v1/billing/reactivation")
        .with_session(sessions::MANAGER)
        .json(&json!({ "payment_method_id": "card_1" }))
        .await;
    no_card.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        no_card.json::<Value>(),
        json!({ "error": "Choose a card to pay with" })
    );

    let foreign = server
        .post("/v1/billing/reactivation")
        .with_session(sessions::MANAGER)
        .json(&json!({ "payment_method_id": "pm_foreign" }))
        .await;
    foreign.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        foreign.json::<Value>(),
        json!({ "error": "This card cannot be used" })
    );

    let started = server
        .post("/v1/billing/reactivation")
        .with_session(sessions::MANAGER)
        .json(&json!({ "payment_method_id": "pm_new" }))
        .await
        .json::<Value>();
    assert_eq!(
        started,
        json!({ "status": "requires_payment", "client_secret": "pi_new_secret_abc", "subscription_id": "sub_new" })
    );
    {
        let fake = stripe.lock().unwrap();
        let find = |method: &str, path: &str| {
            fake.requests
                .iter()
                .find(|(m, p, _)| m == method && p == path)
                .map(|(_, _, body)| form(body))
                .unwrap_or_else(|| panic!("no {method} {path}"))
        };
        assert_eq!(
            find("POST", "/v1/payment_methods/pm_new/attach")["customer"],
            "cus_100"
        );
        assert_eq!(
            find("POST", "/v1/customers/cus_100")["invoice_settings[default_payment_method]"],
            "pm_new"
        );
        let created = find("POST", "/v1/subscriptions");
        assert_eq!(created["items[0][price]"], "price_flat");
        assert_eq!(created["items[0][quantity]"], "1");
        assert_eq!(created["payment_behavior"], "default_incomplete");
        assert_eq!(created["metadata[company_id]"], "100");
        assert_eq!(created["expand[]"], "latest_invoice.confirmation_secret");
    }
    // Still locked until the payment goes through.
    let row = billing_row(&pool, 100).await.unwrap();
    assert_eq!(row.stripe_subscription_id.as_deref(), Some("sub_new"));
    assert_ne!(row.payment_status, "active");

    // The browser confirmed the payment; Stripe now has the subscription active.
    stripe
        .lock()
        .unwrap()
        .subscriptions
        .get_mut("sub_new")
        .unwrap()["status"] = json!("active");
    let finished = server
        .post("/v1/billing/reactivation/finish")
        .with_session(sessions::MANAGER)
        .json(&json!({ "subscription_id": "sub_new" }))
        .await
        .json::<Value>();
    assert_eq!(finished, json!({ "status": "active" }));
    let row: (String, String, i32, i32) = sqlx::query_as(
        "SELECT payment_status, subscription_status, user_count, monthly_price_cents
           FROM company_billing WHERE company_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, ("active".into(), "active".into(), 1, 4900));

    // Paying again finds the paid subscription instead of charging twice.
    let again = server
        .post("/v1/billing/reactivation")
        .with_session(sessions::MANAGER)
        .json(&json!({ "payment_method_id": "pm_new" }))
        .await
        .json::<Value>();
    assert_eq!(again, json!({ "status": "paid" }));
}

#[test_db::test(seed)]
async fn finishing_someone_elses_subscription_is_refused(pool: MySqlPool) {
    let stripe = Shared::default();
    suspended(&pool, &stripe).await;
    stripe.lock().unwrap().subscriptions.insert(
        "sub_theirs".into(),
        json!({ "id": "sub_theirs", "object": "subscription", "status": "active", "customer": "cus_other",
                "items": { "data": [] } }),
    );
    let server = server(&pool, Some(&stripe)).await;
    let response = server
        .post("/v1/billing/reactivation/finish")
        .with_session(sessions::MANAGER)
        .json(&json!({ "subscription_id": "sub_theirs" }))
        .await;
    response.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>(),
        json!({ "error": "This subscription does not belong to your company" })
    );
    // Without a subscription it reports where things stand.
    let pending = server
        .post("/v1/billing/reactivation/finish")
        .with_session(sessions::MANAGER)
        .json(&json!({}))
        .await
        .json::<Value>();
    assert_eq!(pending, json!({ "status": "pending" }));
}

// ---- The billing page: GET /v1/billing, /gate, checkout, portal, plan change

/// Company 100 pays for `sub_100` (active) with card `pm_100`; one paid and
/// one open invoice at Stripe.
async fn subscribed(pool: &MySqlPool, stripe: &Shared) {
    manager_and_billing(pool).await;
    sqlx::query(
        "INSERT INTO company_billing (company_id, stripe_customer_id, stripe_subscription_id, stripe_price_id,
                                      stripe_subscription_item_id, stripe_payment_method_id,
                                      subscription_status, payment_status, plan_name, monthly_price_cents)
         VALUES (100, 'cus_100', 'sub_100', 'price_flat', 'si_100', 'pm_100', 'active', 'active',
                 'Granite Manager', 4900)",
    )
    .execute(pool)
    .await
    .unwrap();
    let mut fake = stripe.lock().unwrap();
    fake.subscriptions.insert(
        "sub_100".into(),
        json!({ "id": "sub_100", "object": "subscription", "status": "active", "customer": "cus_100",
                "default_payment_method": "pm_100", "metadata": { "company_id": "100" },
                "items": { "data": [{ "id": "si_100", "quantity": 1,
                    "current_period_start": 1_788_220_800, "current_period_end": 1_790_899_200,
                    "price": { "id": "price_flat", "nickname": "Granite Manager", "unit_amount": 4900 } }] } }),
    );
    fake.invoices
        .insert("in_paid".into(), invoice("in_paid", true, 4900));
    fake.invoices
        .insert("in_open".into(), invoice("in_open", false, 4900));
    fake.payment_methods.insert(
        "pm_100".into(),
        json!({ "id": "pm_100", "object": "payment_method", "type": "card", "customer": "cus_100",
                "card": { "brand": "visa", "last4": "4242", "exp_month": 4, "exp_year": 2030 } }),
    );
}

#[test_db::test(seed)]
async fn the_billing_page_syncs_from_stripe(pool: MySqlPool) {
    let stripe = Shared::default();
    subscribed(&pool, &stripe).await;
    let server = server(&pool, Some(&stripe)).await;
    let billing: Value = server
        .get("/v1/billing")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(
        billing["subscription"],
        json!({
            "plan_name": "Granite Manager",
            "price_id": "price_flat",
            "monthly_price_cents": 4900,
            "payment_status": "active",
            "subscription_status": "active",
            "current_period_end": "2026-10-02T00:00:00Z",
            // Caught up from the paid invoice (no invoice.paid webhook came).
            "last_successful_payment_at": "2026-09-01T00:00:00Z",
            "failed_payment_at": null,
            "outstanding_balance_cents": 4900
        })
    );
    assert_eq!(
        billing["card"],
        json!({ "id": "pm_100", "brand": "visa", "last4": "4242", "exp_month": 4, "exp_year": 2030 })
    );
    let invoices: Vec<(String, String)> = billing["invoices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["stripe_invoice_id"].as_str().unwrap().into(),
                i["status"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        invoices,
        [
            ("in_paid".into(), "paid".into()),
            ("in_open".into(), "open".into())
        ]
    );
    assert_eq!(billing["invoices"][0]["amount_due_cents"], 0);
    assert_eq!(billing["invoices"][0]["paid_at"], "2026-09-01T00:00:00Z");
    assert_eq!(
        billing["invoices"][1]["hosted_invoice_url"],
        "https://pay.stripe.com/i"
    );
    assert_eq!(
        billing["plans"][1],
        json!({ "price_id": "price_plus", "name": "Granite Manager Plus", "monthly_price_cents": 9900 })
    );
    assert_eq!(billing["restricted"], false);
    assert_eq!(billing["starts_checkout"], false);
    assert_eq!(billing["needs_new_subscription"], false);
    assert_eq!(billing["subscription_ended"], false);
    assert_eq!(billing["billing_configured"], true);
}

#[test_db::test(seed)]
async fn without_a_record_or_stripe_the_page_is_empty(pool: MySqlPool) {
    manager_and_billing(&pool).await;
    let server = server(&pool, None).await;
    let billing: Value = server
        .get("/v1/billing")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(billing["subscription"], Value::Null);
    assert_eq!(billing["card"], Value::Null);
    assert_eq!(billing["invoices"], json!([]));
    assert_eq!(billing["starts_checkout"], false);
    assert_eq!(billing["plan_invite"], Value::Null);
    assert_eq!(billing["billing_configured"], false);
}

#[test_db::test(seed)]
async fn only_managers_see_billing_and_everyone_sees_the_gate(pool: MySqlPool) {
    let stripe = Shared::default();
    suspended(&pool, &stripe).await;
    let server = server(&pool, Some(&stripe)).await;
    for path in ["/v1/billing", "/v1/billing/reactivation"] {
        server
            .get(path)
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        server
            .get(path)
            .with_session(sessions::REP)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    server
        .post("/v1/billing/checkout")
        .with_session(sessions::REP)
        .json(&json!({ "price_id": "price_flat" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let rep: Value = server
        .get("/v1/billing/gate")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        rep,
        json!({ "show_warning": false, "days_remaining": 0, "restricted": true,
                "reason": "payment_failed", "pauses_at": null, "can_manage": false })
    );
    let manager: Value = server
        .get("/v1/billing/gate")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(manager["restricted"], true);
    assert_eq!(manager["can_manage"], true);
    // Another company is not affected.
    let other: Value = server
        .get("/v1/billing/gate")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(other["restricted"], false);
}

#[test_db::test(seed)]
async fn a_suspended_company_gets_the_reactivation_quote(pool: MySqlPool) {
    let stripe = Shared::default();
    suspended(&pool, &stripe).await;
    stripe.lock().unwrap().payment_methods.insert(
        "pm_saved".into(),
        json!({ "id": "pm_saved", "type": "card", "customer": "cus_100",
                "card": { "brand": "mastercard", "last4": "4444", "exp_month": 1, "exp_year": 2031 } }),
    );
    let server = server(&pool, Some(&stripe)).await;
    let billing: Value = server
        .get("/v1/billing")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(billing["restricted"], true);
    assert_eq!(billing["needs_new_subscription"], true);
    assert_eq!(billing["subscription_ended"], false);
    let quote: Value = server
        .get("/v1/billing/reactivation")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(quote["needed"], true);
    assert_eq!(
        quote["plan"],
        json!({ "price_id": "price_flat", "plan_name": "Granite Manager", "quantity": 1, "monthly_cents": 4900 })
    );
    assert_eq!(quote["cards"][0]["id"], "pm_saved");
    assert_eq!(quote["publishable_key"], Value::Null);
}

async fn self_serve(pool: &MySqlPool) {
    sqlx::query("UPDATE company SET plan_source = 'subscription' WHERE id = 100")
        .execute(pool)
        .await
        .unwrap();
}

#[test_db::test(seed)]
async fn a_self_serve_company_whose_subscription_ended_is_sent_to_granite_manager(pool: MySqlPool) {
    let stripe = Shared::default();
    suspended(&pool, &stripe).await;
    self_serve(&pool).await;
    let server = server(&pool, Some(&stripe)).await;
    let billing: Value = server
        .get("/v1/billing")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(billing["restricted"], true);
    assert_eq!(billing["subscription_ended"], true);
    // Never the legacy restore page.
    assert_eq!(billing["needs_new_subscription"], false);
    let quote: Value = server
        .get("/v1/billing/reactivation")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(quote["needed"], false);
    let refused = server
        .post("/v1/billing/reactivation")
        .with_session(sessions::MANAGER)
        .json(&json!({ "payment_method_id": "pm_new" }))
        .await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>(),
        json!({ "error": "This company is on a self-serve plan. Contact Granite Manager to start again." })
    );
    let posted = stripe
        .lock()
        .unwrap()
        .requests
        .iter()
        .any(|r| r.0 == "POST");
    assert!(!posted);
}

#[test_db::test(seed)]
async fn a_running_self_serve_subscription_has_not_ended(pool: MySqlPool) {
    let stripe = Shared::default();
    subscribed(&pool, &stripe).await;
    self_serve(&pool).await;
    let server = server(&pool, Some(&stripe)).await;
    let billing: Value = server
        .get("/v1/billing")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(billing["subscription_ended"], false);
    assert_eq!(billing["needs_new_subscription"], false);
}

#[test_db::test(seed)]
async fn a_paid_up_company_needs_no_reactivation(pool: MySqlPool) {
    let stripe = Shared::default();
    subscribed(&pool, &stripe).await;
    let server = server(&pool, Some(&stripe)).await;
    let quote: Value = server
        .get("/v1/billing/reactivation")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(
        quote,
        json!({ "needed": false, "cards": [], "plan": null, "publishable_key": null })
    );
}

#[test_db::test(seed)]
async fn checkout_no_longer_starts_subscriptions(pool: MySqlPool) {
    manager_and_billing(&pool).await;
    let stripe = Shared::default();
    let server = server(&pool, Some(&stripe)).await;
    let refused = server
        .post("/v1/billing/checkout")
        .with_session(sessions::MANAGER)
        .json(&json!({ "price_id": "price_flat" }))
        .await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        refused.json::<Value>(),
        json!({ "error": "Contact Granite Manager to choose your plan.", "code": "plan_invite_required" })
    );
    assert!(stripe.lock().unwrap().requests.is_empty());
}

#[test_db::test(seed)]
async fn a_subscribed_company_changes_its_plan_instead_of_checking_out(pool: MySqlPool) {
    let stripe = Shared::default();
    subscribed(&pool, &stripe).await;
    let server = server(&pool, Some(&stripe)).await;
    server
        .post("/v1/billing/checkout")
        .with_session(sessions::MANAGER)
        .json(&json!({ "price_id": "price_plus" }))
        .await
        .assert_status(StatusCode::CONFLICT);
    server
        .put("/v1/billing/subscription")
        .with_session(sessions::MANAGER)
        .json(&json!({ "price_id": "price_plus" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let update = stripe
        .lock()
        .unwrap()
        .requests
        .iter()
        .find(|r| r.0 == "POST" && r.1 == "/v1/subscriptions/sub_100")
        .map(|r| form(&r.2))
        .unwrap();
    assert_eq!(update["items[0][id]"], "si_100");
    assert_eq!(update["items[0][price]"], "price_plus");
    assert_eq!(update["proration_behavior"], "create_prorations");
    let (price, plan): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT stripe_price_id, plan_name FROM company_billing WHERE company_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(price.as_deref(), Some("price_plus"));
    assert_eq!(plan.as_deref(), Some("Granite Manager Plus"));
}

#[test_db::test(seed)]
async fn the_portal_opens_the_card_form_or_the_subscription(pool: MySqlPool) {
    let stripe = Shared::default();
    subscribed(&pool, &stripe).await;
    let server = server(&pool, Some(&stripe)).await;
    for flow in [json!({}), json!({ "flow": "manage" })] {
        let link: Value = server
            .post("/v1/billing/portal")
            .with_session(sessions::MANAGER)
            .json(&flow)
            .await
            .json();
        assert_eq!(link["url"], "https://billing.stripe.com/p/session/1");
    }
    let portals: Vec<HashMap<String, String>> = stripe
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.1 == "/v1/billing_portal/sessions")
        .map(|r| form(&r.2))
        .collect();
    assert_eq!(portals[0]["flow_data[type]"], "payment_method_update");
    assert_eq!(
        portals[0]["return_url"],
        format!("{ALLOWED_ORIGIN}/admin/billing")
    );
    assert!(!portals[1].contains_key("flow_data[type]"));
    // No tagged configuration yet: the no-cancel one is created once and
    // used by every session.
    assert!(portals.iter().all(|p| p["configuration"] == "bpc_created"));
    let created: Vec<HashMap<String, String>> = stripe
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.0 == "POST" && r.1 == "/v1/billing_portal/configurations")
        .map(|r| form(&r.2))
        .collect();
    assert_eq!(created.len(), 1);
    let config = &created[0];
    assert_eq!(config["metadata[app]"], "granite-manager-no-cancel-v1");
    assert_eq!(config["features[subscription_cancel][enabled]"], "false");
    assert_eq!(config["features[subscription_update][enabled]"], "false");
    assert_eq!(config["features[payment_method_update][enabled]"], "true");
    assert_eq!(config["features[invoice_history][enabled]"], "true");
    assert_eq!(
        config["features[customer_update][allowed_updates][1]"],
        "address"
    );
}
