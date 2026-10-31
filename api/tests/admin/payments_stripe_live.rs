//! The payments page's routes against Stripe's test mode. Runs only when
//! `STRIPE_TEST_SECRET_KEY` is a `sk_test_` key; it creates a test customer
//! (deleted at the end, with its subscription) and, the first time, the
//! `granite_manager_subscription` product and a portal configuration, which
//! stay for the next runs.

use crate::common;

use api::app;
use api::billing::stripe::{API_VERSION, Stripe};
use api::routes::payments::{PRODUCT_ID, Payments};
use api::state::AppState;
use axum::http::StatusCode;
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::Value;
use sqlx::MySqlPool;

const BASE: &str = "https://api.stripe.com";

fn test_key() -> Option<String> {
    std::env::var("STRIPE_TEST_SECRET_KEY")
        .ok()
        .filter(|key| key.starts_with("sk_test_"))
}

/// Stripe calls the test makes as Stripe or the payer would.
struct Raw {
    key: String,
    http: reqwest::Client,
}

impl Raw {
    async fn call(&self, method: reqwest::Method, path: &str, form: &[(&str, &str)]) -> Value {
        let body = serde_html_form::to_string(form).unwrap();
        let get = method == reqwest::Method::GET || method == reqwest::Method::DELETE;
        let url = if get && !body.is_empty() {
            format!("{BASE}{path}?{body}")
        } else {
            format!("{BASE}{path}")
        };
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(&self.key)
            .header("Stripe-Version", API_VERSION);
        if !get {
            request = request
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(body);
        }
        request.send().await.unwrap().json().await.unwrap()
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Value {
        self.call(reqwest::Method::GET, path, query).await
    }

    async fn post(&self, path: &str, form: &[(&str, &str)]) -> Value {
        self.call(reqwest::Method::POST, path, form).await
    }
}

#[test_db::test(seed)]
async fn a_company_subscribes_manages_and_shows_up_for_superusers(pool: MySqlPool) {
    let Some(key) = test_key() else {
        eprintln!("skipped: STRIPE_TEST_SECRET_KEY is not a sk_test_ key");
        return;
    };
    let raw = Raw {
        key: key.clone(),
        http: ::common::http::client(),
    };
    let payments = Payments::new("live-test-secret", "https://dev-payments.granite-manager.com");
    let token = payments.token(100).unwrap();
    let server = TestServer::new(app(AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_stripe(Stripe::new(Some(&key), BASE))
        .with_extension(payments)));

    // Checkout: a customer is made and saved, the session charges $800 a month.
    let checkout = server
        .post(&format!("/v1/public/payments/{token}/checkout"))
        .await;
    checkout.assert_status_ok();
    let url = checkout.json::<Value>()["url"].as_str().unwrap().to_string();
    assert!(url.starts_with("https://checkout.stripe.com/"), "{url}");
    let customer: String =
        sqlx::query_scalar("SELECT stripe_customer_id FROM company WHERE id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(customer.starts_with("cus_"), "{customer}");
    let session_id = url
        .split('/')
        .find_map(|part| part.split('#').next().filter(|id| id.starts_with("cs_test_")))
        .expect("session id in the Checkout link");
    let session = raw
        .get(
            &format!("/v1/checkout/sessions/{session_id}"),
            &[("expand[]", "line_items")],
        )
        .await;
    assert_eq!(session["mode"], "subscription");
    assert_eq!(session["customer"], customer.as_str());
    assert_eq!(session["client_reference_id"], "100");
    assert_eq!(session["amount_total"], 80000);
    let line = &session["line_items"]["data"][0];
    assert_eq!(line["price"]["product"], PRODUCT_ID);
    assert_eq!(line["price"]["recurring"]["interval"], "month");
    assert_eq!(
        session["success_url"],
        format!("https://dev-payments.granite-manager.com/?c={token}&checkout=success")
    );

    // The payer pays: a subscription like the one Checkout makes.
    let card = raw
        .post(
            "/v1/payment_methods/pm_card_visa/attach",
            &[("customer", &customer)],
        )
        .await;
    let card = card["id"].as_str().unwrap().to_string();
    let subscription = raw
        .post(
            "/v1/subscriptions",
            &[
                ("customer", &customer),
                ("default_payment_method", &card),
                ("items[0][price_data][currency]", "usd"),
                ("items[0][price_data][product]", PRODUCT_ID),
                ("items[0][price_data][unit_amount]", "80000"),
                ("items[0][price_data][recurring][interval]", "month"),
                ("metadata[company_id]", "100"),
            ],
        )
        .await;
    assert_eq!(subscription["status"], "active", "{subscription}");
    let subscription_id = subscription["id"].as_str().unwrap().to_string();

    // The page shows it, and a second checkout is refused.
    let page: Value = server
        .get(&format!("/v1/public/payments/{token}"))
        .await
        .json();
    assert_eq!(page["company_name"], "Acme Stone");
    assert_eq!(page["subscription"]["id"], subscription_id.as_str());
    assert_eq!(page["subscription"]["status"], "active");
    assert_eq!(page["subscription"]["price_cents"], 80000);
    assert_eq!(page["subscription"]["renews"], true);
    assert!(page["subscription"]["next_payment_at"].is_string());
    server
        .post(&format!("/v1/public/payments/{token}/checkout"))
        .await
        .assert_status(StatusCode::CONFLICT);

    // The portal opens (its configuration is made if the account has none).
    let portal = server
        .post(&format!("/v1/public/payments/{token}/portal"))
        .await;
    portal.assert_status_ok();
    let portal_url = portal.json::<Value>()["url"].as_str().unwrap().to_string();
    assert!(portal_url.starts_with("https://billing.stripe.com/"), "{portal_url}");

    // Superusers see the payment.
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(&pool)
        .await
        .unwrap();
    let companies: Value = server
        .get("/v1/superuser/payments")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let acme = companies
        .as_array()
        .unwrap()
        .iter()
        .find(|company| company["id"] == 100)
        .unwrap();
    assert_eq!(acme["stripe_customer_id"], customer.as_str());
    assert_eq!(acme["last_payment_cents"], 80000);
    assert!(acme["last_payment_at"].is_string());
    assert_eq!(acme["subscription"]["renews"], true);
    assert_eq!(acme["stripe_error"], Value::Null);

    // Cancelled in the portal: it runs to the end of the month, then stops.
    raw.post(
        &format!("/v1/subscriptions/{subscription_id}"),
        &[("cancel_at_period_end", "true")],
    )
    .await;
    let page: Value = server
        .get(&format!("/v1/public/payments/{token}"))
        .await
        .json();
    assert_eq!(page["subscription"]["renews"], false);
    assert!(page["subscription"]["ends_at"].is_string());

    let deleted = raw
        .call(reqwest::Method::DELETE, &format!("/v1/customers/{customer}"), &[])
        .await;
    assert_eq!(deleted["deleted"], true);
}
