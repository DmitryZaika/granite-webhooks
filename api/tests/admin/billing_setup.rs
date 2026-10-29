use crate::common;

use api::app;
use api::billing::stripe::{API_VERSION, Stripe};
use api::billing::{BillingPlan, setup::REQUIRED_EVENTS};
use api::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone)]
struct Fake {
    endpoints: Value,
    prices: Arc<HashMap<String, Value>>,
}

async fn fake_stripe(fake: Fake) -> String {
    let router =
        Router::new()
            .route(
                "/v1/webhook_endpoints",
                get(|State(f): State<Fake>| async move {
                    Json(json!({ "object": "list", "data": f.endpoints }))
                }),
            )
            .route(
                "/v1/prices/{id}",
                get(
                    |State(f): State<Fake>,
                     axum::extract::Path(id): axum::extract::Path<String>| async move {
                        f.prices.get(&id).cloned().map(Json).ok_or((
                            StatusCode::NOT_FOUND,
                            Json(json!({ "error": { "message": "No such price" } })),
                        ))
                    },
                ),
            )
            .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn price(active: bool, interval: &str) -> Value {
    json!({ "id": "p", "active": active, "currency": "usd", "livemode": false,
        "unit_amount": 4900, "recurring": { "interval": interval, "interval_count": 1 } })
}

fn endpoint(events: &[&str], version: &str) -> Value {
    json!([{ "url": "https://api.granite-manager.com/v1/webhooks/stripe", "status": "enabled",
        "enabled_events": events, "api_version": version }])
}

async fn server(pool: &MySqlPool, fake: Fake, plans: &[&str]) -> TestServer {
    sqlx::raw_sql("UPDATE users SET is_superuser = 1 WHERE id = 103")
        .execute(pool)
        .await
        .unwrap();
    let plans = plans
        .iter()
        .map(|id| BillingPlan {
            price_id: (*id).into(),
            name: format!("Plan {id}"),
            monthly_price_cents: Some(4900),
        })
        .collect();
    let stripe = Stripe::new(Some("sk_test_fake"), &fake_stripe(fake).await);
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_stripe(stripe)
        .with_billing_plans(plans);
    TestServer::new(app(state))
}

async fn report(server: &TestServer) -> HashMap<String, (bool, String)> {
    let body: Value = server
        .get("/v1/superuser/billing/setup")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert!(body["checked_at"].is_string());
    body["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["key"].as_str().unwrap().to_string(),
                (
                    c["ok"].as_bool().unwrap(),
                    c["detail"].as_str().unwrap().to_string(),
                ),
            )
        })
        .collect()
}

fn fake(endpoints: Value, prices: &[(&str, Value)]) -> Fake {
    Fake {
        endpoints,
        prices: Arc::new(
            prices
                .iter()
                .map(|(id, v)| ((*id).to_string(), v.clone()))
                .collect(),
        ),
    }
}

#[test_db::test(seed)]
async fn only_super_admins_can_read_the_setup(pool: MySqlPool) {
    let server = server(&pool, fake(json!([]), &[]), &[]).await;
    server
        .get("/v1/superuser/billing/setup")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .get("/v1/superuser/billing/setup")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn a_good_setup_passes_the_stripe_checks(pool: MySqlPool) {
    let f = fake(
        endpoint(&REQUIRED_EVENTS, API_VERSION),
        &[("price_a", price(true, "month"))],
    );
    let checks = report(&server(&pool, f, &["price_a"]).await).await;
    for key in [
        "webhook_endpoint",
        "webhook_events",
        "webhook_api_version",
        "price_price_a",
    ] {
        assert!(checks[key].0, "{key}: {}", checks[key].1);
    }
}

#[test_db::test(seed)]
async fn the_report_carries_the_startup_configuration_check(pool: MySqlPool) {
    let f = fake(endpoint(&REQUIRED_EVENTS, API_VERSION), &[]);
    let checks = report(&server(&pool, f, &[]).await).await;
    let (ok, detail) = &checks["startup_config"];
    // The test process lacks the production variables.
    assert!(!ok);
    assert!(!detail.is_empty());
    assert!(!detail.contains("sk_test_fake"));
}

#[test_db::test(seed)]
async fn a_wrong_api_version_or_missing_event_fails(pool: MySqlPool) {
    let events: Vec<&str> = REQUIRED_EVENTS
        .into_iter()
        .filter(|e| *e != "invoice.paid")
        .collect();
    let f = fake(endpoint(&events, "2020-01-01"), &[]);
    let checks = report(&server(&pool, f, &[]).await).await;
    assert!(checks["webhook_endpoint"].0);
    assert!(!checks["webhook_api_version"].0);
    assert!(checks["webhook_api_version"].1.contains("2020-01-01"));
    assert!(!checks["webhook_events"].0);
    assert!(checks["webhook_events"].1.contains("invoice.paid"));
}

#[test_db::test(seed)]
async fn inactive_yearly_and_unknown_prices_fail_one_check_each(pool: MySqlPool) {
    let f = fake(
        endpoint(&["*"], API_VERSION),
        &[
            ("price_off", price(false, "month")),
            ("price_year", price(true, "year")),
        ],
    );
    let checks = report(&server(&pool, f, &["price_off", "price_year", "price_gone"]).await).await;
    assert!(checks["price_price_off"].1.contains("inactive"));
    assert!(checks["price_price_year"].1.contains("not a monthly price"));
    assert!(checks["price_price_gone"].1.contains("No such price"));
    assert!(
        ["price_price_off", "price_price_year", "price_price_gone"]
            .iter()
            .all(|k| !checks[*k].0)
    );
}

mod release {
    use super::{WithSession, report, sessions};
    use crate::common::{ALLOWED_ORIGIN, SECRET};
    use crate::fake_stripe::{self, FakeStripe, Shared};
    use api::app;
    use api::billing::setup::REQUIRED_EVENTS;
    use api::billing::stripe::API_VERSION;
    use api::signup::SignupConfig;
    use api::state::AppState;
    use axum_test::TestServer;
    use serde_json::{Value, json};
    use sqlx::MySqlPool;

    fn configuration(bank: bool) -> Value {
        json!({ "is_default": true,
            "card": { "available": true, "display_preference": { "value": "on" } },
            "us_bank_account": { "available": bank,
                "display_preference": { "value": if bank { "on" } else { "off" } } } })
    }

    fn portal(cancel: bool) -> Value {
        json!({ "id": "bpc_1", "active": true,
            "metadata": { "app": "granite-manager-no-cancel-v1" },
            "features": { "subscription_cancel": { "enabled": cancel },
                "subscription_update": { "enabled": false } } })
    }

    async fn server(pool: &MySqlPool, shared: &Shared, configured: bool) -> TestServer {
        sqlx::raw_sql("UPDATE users SET is_superuser = 1 WHERE id = 103")
            .execute(pool)
            .await
            .unwrap();
        let mut state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
            .with_stripe(fake_stripe::client(shared).await);
        if configured {
            state = state
                .with_onboarding_call(api::onboarding_call::Config {
                    calendly: api::onboarding_call::calendly::Calendly::new(
                        Some("tok"),
                        "http://127.0.0.1:1",
                        None,
                    ),
                    ..api::onboarding_call::Config::disabled()
                })
                .with_billing_alert_email("alerts@example.com")
                .with_stripe_webhook_secret("whsec_x")
                .with_signup(SignupConfig {
                    publishable_key: Some("pk_test_x".into()),
                    notify_email: Some("dima@example.com".into()),
                    ..SignupConfig::default()
                });
        }
        TestServer::new(app(state))
    }

    #[test_db::test(seed)]
    async fn a_ready_release_passes_the_stripe_and_price_checks(pool: MySqlPool) {
        let shared = FakeStripe::with_catalog();
        {
            let mut fake = shared.lock().unwrap();
            fake.payment_method_configurations.push(configuration(true));
            fake.portal_configurations.push(portal(false));
            fake.webhook_endpoints.push(json!({
                "url": "https://api.granite-manager.com/v1/webhooks/stripe", "status": "enabled",
                "enabled_events": REQUIRED_EVENTS, "api_version": API_VERSION }));
        }
        let checks = report(&server(&pool, &shared, true).await).await;
        for key in [
            "webhook_events",
            "webhook_api_version",
            "payment_method_types",
            "portal_configuration",
            "billing_alert_email",
            "signup_notify",
            "signup_dependencies",
            "signup_price_granite_base_monthly_v1",
            "signup_price_addon_quickbooks_monthly_v1",
        ] {
            assert!(checks[key].0, "{key}: {}", checks[key].1);
        }
    }

    #[test_db::test(seed)]
    async fn the_release_gaps_are_reported(pool: MySqlPool) {
        let shared = FakeStripe::with_catalog();
        {
            let mut fake = shared.lock().unwrap();
            fake.payment_method_configurations
                .push(configuration(false));
            fake.portal_configurations.push(portal(true));
            fake.prices[0]["tax_behavior"] = json!("unspecified");
        }
        let checks = report(&server(&pool, &shared, false).await).await;
        assert!(checks["payment_method_types"].1.contains("us_bank_account"));
        assert!(!checks["portal_configuration"].0);
        assert!(
            checks["portal_configuration"]
                .1
                .contains("subscription_cancel")
        );
        assert!(!checks["signup_price_granite_base_monthly_v1"].0);
        assert!(
            checks["signup_price_granite_base_monthly_v1"]
                .1
                .contains("tax_behavior")
        );
        for key in [
            "billing_alert_email",
            "signup_notify",
            "signup_dependencies",
        ] {
            assert!(!checks[key].0, "{key}");
        }
    }

    #[test_db::test(seed)]
    async fn a_stripe_error_reads_as_could_not_check(pool: MySqlPool) {
        let shared = FakeStripe::with_catalog();
        shared.lock().unwrap().fail(
            axum::http::Method::GET,
            "/v1/payment_method_configurations",
            fake_stripe::FailMode::Before,
            1,
        );
        let checks = report(&server(&pool, &shared, true).await).await;
        assert!(!checks["payment_method_types"].0);
        assert!(checks["payment_method_types"].1.contains("could not check"));
    }

    #[test_db::test(seed)]
    async fn only_super_admins_see_the_release_checks(pool: MySqlPool) {
        let shared = FakeStripe::with_catalog();
        let server = server(&pool, &shared, true).await;
        server
            .get("/v1/superuser/billing/setup")
            .with_session(sessions::ADMIN)
            .await
            .assert_status(axum::http::StatusCode::FORBIDDEN);
    }
}
