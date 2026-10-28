//! Card and bank account payment methods (step 13): `POST /v1/billing/payment-setup`,
//! `/payment-method`, `/invoice-payment`, the new Stripe webhook events, holds, the
//! gate's bank verification window and the account page's new fields, against a
//! fake Stripe and a fake email sink. Companies 20750 (Pm Stone, paying) and 20751
//! (Pm Other Stone, no Stripe customer), inserted here.

use crate::common::{ALLOWED_ORIGIN, SECRET, WithSession};
use crate::{fake_stripe, fake_webhooks};

use api::app;
use api::billing::methods::attempt_for_event;
use api::billing::stripe::signature_header;
use api::signup::SignupConfig;
use api::state::AppState;
use axum::http::StatusCode;
use axum_test::{TestResponse, TestServer};
use chrono::NaiveDateTime;
use fake_stripe::{FakeStripe, bank_account, card};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::Arc;

const COMPANY: i32 = 20750;
const OTHER_COMPANY: i32 = 20751;
const MANAGER: &str = "aaaaaaaa-0000-4000-8000-000000020750";
const REP: &str = "aaaaaaaa-0000-4000-8000-000000020751";
const OTHER_MANAGER: &str = "aaaaaaaa-0000-4000-8000-000000020752";
const MANAGER_EMAIL: &str = "manager@pm.test";
const ALERT_EMAIL: &str = "ops@example.test";
const CUSTOMER: &str = "cus_pm";
const SUBSCRIPTION: &str = "sub_pm";
const WEBHOOK_SECRET: &str = "whsec_pm_test";
const UUID: &str = "6f1c1d1e-2a3b-4c5d-8e9f-0a1b2c3d4e5f";

struct Fixture {
    pool: MySqlPool,
    server: TestServer,
    stripe: fake_stripe::Shared,
    sink: fake_webhooks::Shared,
}

#[derive(Debug, sqlx::FromRow)]
struct Methods {
    stripe_payment_method_id: Option<String>,
    pending_setup_intent_id: Option<String>,
    method_hold: Option<String>,
    method_hold_until: Option<NaiveDateTime>,
    payment_processing_at: Option<NaiveDateTime>,
    payment_status: String,
}

async fn seed(pool: &MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO company (id, name, plan_source, billing_required, billing_required_at) VALUES
           (20750, 'Pm Stone', 'subscription', 1, UTC_TIMESTAMP() - INTERVAL 30 DAY),
           (20751, 'Pm Other Stone', 'subscription', 1, UTC_TIMESTAMP() - INTERVAL 1 DAY);
         INSERT INTO company_billing
           (company_id, stripe_customer_id, stripe_subscription_id, stripe_payment_method_id,
            subscription_status, payment_status, monthly_price_cents) VALUES
           (20750, 'cus_pm', 'sub_pm', 'pm_old', 'active', 'active', 37500);
         INSERT INTO users (id, email, password, name, is_employee, is_admin, is_superuser, is_deleted, company_id) VALUES
           (20750, 'manager@pm.test', 'x', 'Dana Manager', 1, 1, 0, 0, 20750),
           (20751, 'rep@pm.test',     'x', 'Remy Rep',     1, 0, 0, 0, 20750),
           (20752, 'manager@pm-other.test', 'x', 'Otto Manager', 1, 1, 0, 0, 20751);
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES
           (20750, 5, 20750), (20751, 1, 20750), (20752, 5, 20751);
         INSERT INTO sessions (id, user_id, expiration_date, is_deleted, created_date) VALUES
           ('aaaaaaaa-0000-4000-8000-000000020750', 20750, '2037-12-31 00:00:00', 0, CURRENT_TIMESTAMP),
           ('aaaaaaaa-0000-4000-8000-000000020751', 20751, '2037-12-31 00:00:00', 0, CURRENT_TIMESTAMP),
           ('aaaaaaaa-0000-4000-8000-000000020752', 20752, '2037-12-31 00:00:00', 0, CURRENT_TIMESTAMP);",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// A SetupIntent of the fake, made by the account page.
fn setup_intent(id: &str, company: i32, customer: &str, purpose: &str, status: &str, method: Option<&str>) -> Value {
    json!({
        "id": id, "object": "setup_intent", "customer": customer, "status": status,
        "payment_method": method, "created": 1_800_000_000, "next_action": null,
        "client_secret": format!("{id}_secret_fake"),
        "metadata": { "company_id": company.to_string(), "purpose": purpose },
    })
}

fn invoice(id: &str, created: i64) -> Value {
    json!({
        "id": id, "object": "invoice", "customer": CUSTOMER, "status": "open",
        "amount_due": 37_500, "amount_remaining": 37_500, "amount_paid": 0, "created": created,
        "currency": "usd", "hosted_invoice_url": null, "invoice_pdf": null,
        "status_transitions": { "paid_at": null },
        "parent": { "subscription_details": { "subscription": SUBSCRIPTION,
            "metadata": { "company_id": COMPANY.to_string() } } },
    })
}

fn event(id: &str, kind: &str, object: Value) -> Value {
    json!({ "id": id, "object": "event", "type": kind, "data": { "object": object } })
}

async fn fixture_with(pool: MySqlPool, alert: bool, signup: SignupConfig) -> Fixture {
    seed(&pool).await;
    let stripe = FakeStripe::with_catalog();
    {
        let mut fake = stripe.lock().unwrap();
        fake.customers.insert(
            CUSTOMER.into(),
            json!({ "id": CUSTOMER, "object": "customer", "metadata": { "company_id": "20750" },
                "invoice_settings": { "default_payment_method": "pm_old" } }),
        );
        fake.subscriptions.insert(
            SUBSCRIPTION.into(),
            json!({ "id": SUBSCRIPTION, "object": "subscription", "customer": CUSTOMER,
                "status": "active", "default_payment_method": "pm_old",
                "metadata": { "company_id": "20750" }, "items": { "object": "list", "data": [] } }),
        );
        let mut old = card("pm_old", "mastercard", "1111");
        old["customer"] = json!(CUSTOMER);
        fake.payment_methods.insert("pm_old".into(), old);
        fake.payment_methods
            .insert("pm_card".into(), card("pm_card", "visa", "4242"));
        fake.payment_methods.insert(
            "pm_bank".into(),
            bank_account("pm_bank", "STRIPE TEST BANK", "6789"),
        );
    }
    let sink = fake_webhooks::Shared::default();
    let mut state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_stripe(fake_stripe::client(&stripe).await)
        .with_stripe_webhook_secret(WEBHOOK_SECRET)
        .with_webhooks(fake_webhooks::start(&sink).await)
        .with_signup(signup);
    if alert {
        state = state.with_billing_alert_email(ALERT_EMAIL);
    }
    Fixture {
        pool,
        server: TestServer::new(app(state)),
        stripe,
        sink,
    }
}

async fn fixture(pool: MySqlPool) -> Fixture {
    fixture_with(pool, true, stripe_signup()).await
}

fn stripe_signup() -> SignupConfig {
    SignupConfig {
        publishable_key: Some(Arc::from("pk_test_fake")),
        ..SignupConfig::default()
    }
}

impl Fixture {
    fn fake(&self) -> std::sync::MutexGuard<'_, FakeStripe> {
        self.stripe.lock().unwrap()
    }

    fn add_setup(&self, id: &str, purpose: &str, status: &str, method: Option<&str>) {
        self.fake().setup_intents.insert(
            id.into(),
            setup_intent(id, COMPANY, CUSTOMER, purpose, status, method),
        );
    }

    async fn apply(&self, session: &str, setup_intent_id: &str) -> TestResponse {
        self.server
            .post("/v1/billing/payment-method")
            .with_session(session)
            .json(&json!({ "setup_intent_id": setup_intent_id }))
            .await
    }

    async fn pay(&self, attempt_id: &str) -> TestResponse {
        self.server
            .post("/v1/billing/invoice-payment")
            .with_session(MANAGER)
            .json(&json!({ "attempt_id": attempt_id }))
            .await
    }

    async fn post_event(&self, event: &Value) -> TestResponse {
        let payload = serde_json::to_vec(event).unwrap();
        let header = signature_header(&payload, WEBHOOK_SECRET, chrono::Utc::now().timestamp());
        self.server
            .post("/v1/webhooks/stripe")
            .add_header("stripe-signature", header)
            .add_header("content-type", "application/json")
            .bytes(payload.into())
            .await
    }

    async fn methods(&self) -> Methods {
        sqlx::query_as(
            "SELECT stripe_payment_method_id, pending_setup_intent_id,
                    CAST(method_hold AS CHAR) AS method_hold, method_hold_until,
                    payment_processing_at, CAST(payment_status AS CHAR) AS payment_status
               FROM company_billing WHERE company_id = ?",
        )
        .bind(COMPANY)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn sql(&self, statement: &str) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(statement.to_string()))
            .execute(&self.pool)
            .await
            .unwrap();
    }

    async fn gate(&self, session: &str) -> Value {
        let res = self.server.get("/v1/billing/gate").with_session(session).await;
        res.assert_status_ok();
        res.json()
    }

    fn emails_to(&self, to: &str) -> Vec<Value> {
        self.sink.lock().unwrap().emails_to(to)
    }

    async fn past_due(&self) {
        self.sql(
            "UPDATE company_billing SET payment_status = 'past_due', subscription_status = 'past_due',
                    failed_payment_at = UTC_TIMESTAMP(),
                    grace_period_ends_at = UTC_TIMESTAMP() + INTERVAL 7 DAY
              WHERE company_id = 20750",
        )
        .await;
    }
}

fn subjects(emails: &[Value]) -> Vec<String> {
    emails
        .iter()
        .map(|email| email["subject"].as_str().unwrap_or_default().to_string())
        .collect()
}

// ---- POST /v1/billing/payment-method

#[test_db::test(seed)]
async fn a_new_card_becomes_the_default_and_the_old_one_is_detached(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.add_setup("seti_card", "payment_method_update", "succeeded", Some("pm_card"));

    let res = f.apply(MANAGER, "seti_card").await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["status"], "saved");
    assert_eq!(body["payment_method"]["type"], "card");
    assert_eq!(body["payment_method"]["last4"], "4242");
    assert_eq!(body["verify_url"], Value::Null);

    let methods = f.methods().await;
    assert_eq!(methods.stripe_payment_method_id.as_deref(), Some("pm_card"));
    assert_eq!(methods.pending_setup_intent_id, None);
    {
        let fake = f.fake();
        assert_eq!(
            fake.customers[CUSTOMER]["invoice_settings"]["default_payment_method"],
            "pm_card"
        );
        assert_eq!(fake.subscriptions[SUBSCRIPTION]["default_payment_method"], "pm_card");
        assert_eq!(fake.payment_methods["pm_old"]["customer"], Value::Null);
        assert_eq!(
            fake.keys("POST", "/v1/payment_methods/pm_old/detach"),
            ["pm-detach:seti_card:pm_old"]
        );
        assert_eq!(
            fake.keys("POST", &format!("/v1/customers/{CUSTOMER}")),
            ["pm-default:seti_card:customer"]
        );
    }

    // The same SetupIntent again: the same answer, nothing new at Stripe.
    let again: Value = f.apply(MANAGER, "seti_card").await.json();
    assert_eq!(again, body);
    assert_eq!(f.fake().calls("POST", "/v1/payment_methods/pm_old/detach").len(), 1);
}

#[test_db::test(seed)]
async fn a_bank_account_waiting_for_microdeposits_is_kept_pending(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.add_setup("seti_bank", "payment_method_update", "requires_payment_method", None);
    f.fake().pend_microdeposits("seti_bank", "pm_bank");

    let body: Value = f.apply(MANAGER, "seti_bank").await.json();
    assert_eq!(body["status"], "verifying");
    assert_eq!(body["payment_method"]["type"], "us_bank_account");
    assert_eq!(body["payment_method"]["last4"], "6789");
    assert_eq!(body["verify_url"], "https://payments.stripe.com/microdeposit/fake");
    assert!(body["verify_by"].is_string());
    let methods = f.methods().await;
    assert_eq!(methods.stripe_payment_method_id.as_deref(), Some("pm_old"));
    assert_eq!(methods.pending_setup_intent_id.as_deref(), Some("seti_bank"));

    let view: Value = f
        .server
        .get("/v1/billing/subscription")
        .with_session(MANAGER)
        .await
        .json();
    assert_eq!(view["pending_payment_method"]["status"], "verifying");
    assert_eq!(view["publishable_key"], "pk_test_fake");

    // Stripe verifies the deposits: the webhook makes it the default.
    f.fake().verify_microdeposits("seti_bank");
    let verified = event("evt_bank_ok", "setup_intent.succeeded", json!({ "id": "seti_bank" }));
    f.post_event(&verified).await.assert_status_ok();
    let methods = f.methods().await;
    assert_eq!(methods.stripe_payment_method_id.as_deref(), Some("pm_bank"));
    assert_eq!(methods.pending_setup_intent_id, None);
    let detaches = f.fake().calls("POST", "/v1/payment_methods/pm_old/detach").len();
    assert_eq!(detaches, 1);

    // The same event again changes nothing.
    f.post_event(&verified).await.assert_status_ok();
    assert_eq!(f.fake().calls("POST", "/v1/payment_methods/pm_old/detach").len(), 1);
    // The browser coming back afterwards gets the same saved answer.
    let late: Value = f.apply(MANAGER, "seti_bank").await.json();
    assert_eq!(late["status"], "saved");
}

#[test_db::test(seed)]
async fn a_new_card_replaces_a_pending_bank_account_and_cancels_it(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.add_setup("seti_bank", "payment_method_update", "requires_payment_method", None);
    f.fake().pend_microdeposits("seti_bank", "pm_bank");
    f.apply(MANAGER, "seti_bank").await.assert_status_ok();
    f.add_setup("seti_card", "payment_method_update", "succeeded", Some("pm_card"));

    assert_eq!(f.apply(MANAGER, "seti_card").await.json::<Value>()["status"], "saved");

    assert_eq!(f.methods().await.pending_setup_intent_id, None);
    assert_eq!(f.fake().setup_intents["seti_bank"]["status"], "canceled");
}

#[test_db::test(seed)]
async fn an_unconfirmed_setup_is_failed_and_changes_nothing(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.add_setup("seti_new", "payment_method_update", "requires_payment_method", None);
    let body: Value = f.apply(MANAGER, "seti_new").await.json();
    assert_eq!(body["status"], "failed");
    assert_eq!(body["payment_method"], Value::Null);
    let methods = f.methods().await;
    assert_eq!(methods.stripe_payment_method_id.as_deref(), Some("pm_old"));
    assert_eq!(methods.pending_setup_intent_id, None);
}

#[test_db::test(seed)]
async fn only_managers_and_only_the_companys_own_setup(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.add_setup("seti_card", "payment_method_update", "succeeded", Some("pm_card"));
    f.apply(REP, "seti_card").await.assert_status(StatusCode::FORBIDDEN);
    f.server
        .post("/v1/billing/invoice-payment")
        .with_session(REP)
        .json(&json!({ "attempt_id": UUID }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    f.server
        .post("/v1/billing/payment-setup")
        .with_session(REP)
        .json(&json!({ "purpose": "update" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    f.server
        .post("/v1/billing/payment-method")
        .json(&json!({ "setup_intent_id": "seti_card" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    // Company 20750's SetupIntent, applied by the other company's Manager.
    let res = f.apply(OTHER_MANAGER, "seti_card").await;
    res.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(res.json::<Value>()["code"], "setup_not_found");
    // Metadata naming the company, on another customer.
    f.fake().setup_intents.insert(
        "seti_foreign".into(),
        setup_intent("seti_foreign", COMPANY, "cus_elsewhere", "payment_method_update", "succeeded", Some("pm_card")),
    );
    f.apply(MANAGER, "seti_foreign").await.assert_status(StatusCode::NOT_FOUND);
    f.apply(MANAGER, "seti_missing").await.assert_status(StatusCode::NOT_FOUND);
    f.apply(MANAGER, "pm_card").await.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(f.methods().await.stripe_payment_method_id.as_deref(), Some("pm_old"));
}

// ---- POST /v1/billing/payment-setup

#[test_db::test(seed)]
async fn payment_setup_makes_a_company_setup_intent(pool: MySqlPool) {
    let f = fixture(pool).await;
    let res = f
        .server
        .post("/v1/billing/payment-setup")
        .with_session(MANAGER)
        .json(&json!({ "purpose": "pay" }))
        .await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["mode"], "stripe");
    assert_eq!(body["publishable_key"], "pk_test_fake");
    let created = f.fake().calls("POST", "/v1/setup_intents");
    assert_eq!(created.len(), 1);
    let seen = &created[0];
    assert_eq!(seen.field("metadata[company_id]").as_deref(), Some("20750"));
    assert_eq!(seen.field("metadata[purpose]").as_deref(), Some("pay"));
    assert_eq!(seen.field("customer").as_deref(), Some(CUSTOMER));
    assert_eq!(seen.field("usage").as_deref(), Some("off_session"));
    let types: Vec<String> = seen
        .fields()
        .into_iter()
        .filter(|(key, _)| key == "payment_method_types[]")
        .map(|(_, value)| value)
        .collect();
    assert_eq!(types, ["card", "us_bank_account"]);
    assert!(
        seen.idempotency_key
            .as_deref()
            .is_some_and(|key| key.starts_with("billing-setup:20750:"))
    );

    // No Stripe customer: 409 no_customer.
    let res = f
        .server
        .post("/v1/billing/payment-setup")
        .with_session(OTHER_MANAGER)
        .json(&json!({ "purpose": "update" }))
        .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(res.json::<Value>()["code"], "no_customer");
    f.server
        .post("/v1/billing/payment-setup")
        .with_session(MANAGER)
        .json(&json!({ "purpose": "nonsense" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn the_test_seam_saves_a_test_card_at_once(pool: MySqlPool) {
    let signup = SignupConfig {
        test_payment_method: Some(Arc::from("pm_card")),
        ..SignupConfig::default()
    };
    let f = fixture_with(pool, false, signup).await;
    let body: Value = f
        .server
        .post("/v1/billing/payment-setup")
        .with_session(MANAGER)
        .json(&json!({ "purpose": "update" }))
        .await
        .json();
    assert_eq!(body, json!({ "mode": "test" }));
    assert_eq!(f.methods().await.stripe_payment_method_id.as_deref(), Some("pm_card"));
}

#[test_db::test(seed)]
async fn without_a_publishable_key_payments_are_unavailable(pool: MySqlPool) {
    let f = fixture_with(pool, false, SignupConfig::default()).await;
    let res = f
        .server
        .post("/v1/billing/payment-setup")
        .with_session(MANAGER)
        .json(&json!({ "purpose": "update" }))
        .await;
    res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(res.json::<Value>()["code"], "payments_unavailable");
}

// ---- POST /v1/billing/invoice-payment (D17)

#[test_db::test(seed)]
async fn a_card_pays_the_oldest_invoice_but_only_stripe_restores_access(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.past_due().await;
    f.fake().invoices = vec![invoice("in_new", 200), invoice("in_old", 100)];

    let body: Value = f.pay(UUID).await.json();
    assert_eq!(body["status"], "paid");
    assert_eq!(body["amount_cents"], 37_500);
    assert_eq!(
        f.fake().keys("POST", "/v1/invoices/in_old/pay"),
        [format!("invoice-pay:in_old:{UUID}")]
    );
    assert_eq!(
        f.fake().calls("POST", "/v1/invoices/in_old/pay")[0]
            .field("payment_method")
            .as_deref(),
        Some("pm_old")
    );
    // The browser's answer never leaves past_due.
    assert_eq!(f.methods().await.payment_status, "past_due");

    let paid = f.fake().invoices.iter().find(|i| i["id"] == "in_old").cloned().unwrap();
    f.post_event(&event("evt_paid", "invoice.paid", paid)).await.assert_status_ok();
    assert_eq!(f.methods().await.payment_status, "active");
}

#[test_db::test(seed)]
async fn a_bank_payment_is_processing_until_stripe_says_paid(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.past_due().await;
    f.sql("UPDATE company_billing SET stripe_payment_method_id = 'pm_bank' WHERE company_id = 20750")
        .await;
    f.fake().invoices = vec![invoice("in_1", 100)];

    let body: Value = f.pay(UUID).await.json();
    assert_eq!(body["status"], "processing");
    let methods = f.methods().await;
    assert!(methods.payment_processing_at.is_some());
    assert_eq!(methods.payment_status, "past_due");
    let view: Value = f.server.get("/v1/billing/subscription").with_session(MANAGER).await.json();
    assert_eq!(view["payment_processing"], true);
    assert_eq!(view["amount_due_cents"], 0);

    // invoice.payment_failed (a returned debit) ends the payment in flight.
    f.post_event(&event("evt_failed", "invoice.payment_failed", invoice("in_1", 100)))
        .await
        .assert_status_ok();
    assert_eq!(f.methods().await.payment_processing_at, None);
}

#[test_db::test(seed)]
async fn three_d_secure_decline_and_nothing_due(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.past_due().await;
    {
        let mut fake = f.fake();
        fake.invoices = vec![invoice("in_3ds", 100)];
        fake.requires_action_invoices = vec!["in_3ds".into()];
    }
    let body: Value = f.pay(UUID).await.json();
    assert_eq!(body["status"], "requires_action");
    assert_eq!(body["client_secret"], "pi_in_3ds_secret_fake");

    {
        let mut fake = f.fake();
        fake.requires_action_invoices.clear();
        fake.decline_next_pay = true;
    }
    let body: Value = f.pay("7f1c1d1e-2a3b-4c5d-8e9f-0a1b2c3d4e5f").await.json();
    assert_eq!(body["status"], "declined");
    assert_eq!(body["decline_message"], "Your card was declined.");
    assert_eq!(f.methods().await.payment_status, "past_due");

    f.fake().invoices.clear();
    let body: Value = f.pay("8f1c1d1e-2a3b-4c5d-8e9f-0a1b2c3d4e5f").await.json();
    assert_eq!(body["status"], "nothing_due");
    assert_eq!(body["amount_cents"], Value::Null);

    f.pay("not-a-uuid").await.assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn the_onboarding_fee_is_never_paid_here_and_no_method_is_409(pool: MySqlPool) {
    let f = fixture(pool).await;
    let mut fee = invoice("in_fee", 50);
    fee["metadata"] = json!({ "kind": "onboarding_fee" });
    f.fake().invoices = vec![fee];
    assert_eq!(f.pay(UUID).await.json::<Value>()["status"], "nothing_due");

    f.sql("UPDATE company_billing SET stripe_payment_method_id = NULL WHERE company_id = 20750").await;
    f.fake().customers.get_mut(CUSTOMER).unwrap()["invoice_settings"]["default_payment_method"] =
        Value::Null;
    let res = f.pay(UUID).await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(res.json::<Value>()["code"], "no_payment_method");
}

// ---- webhooks

#[test_db::test(seed)]
async fn a_verified_bank_on_the_pay_page_pays_the_overdue_invoice(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.past_due().await;
    f.add_setup("seti_pay", "pay", "requires_payment_method", None);
    f.fake().pend_microdeposits("seti_pay", "pm_bank");
    f.fake().invoices = vec![invoice("in_due", 100)];
    assert_eq!(f.apply(MANAGER, "seti_pay").await.json::<Value>()["status"], "verifying");

    f.fake().verify_microdeposits("seti_pay");
    f.post_event(&event("evt_pay_ok", "setup_intent.succeeded", json!({ "id": "seti_pay" })))
        .await
        .assert_status_ok();
    assert_eq!(
        f.fake().keys("POST", "/v1/invoices/in_due/pay"),
        [format!("invoice-pay:in_due:{}", attempt_for_event("evt_pay_ok"))]
    );
    let methods = f.methods().await;
    assert_eq!(methods.stripe_payment_method_id.as_deref(), Some("pm_bank"));
    assert!(methods.payment_processing_at.is_some());
    assert_eq!(methods.payment_status, "past_due");
}

#[test_db::test(seed)]
async fn a_succeeded_card_not_pending_is_left_to_the_browser(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.add_setup("seti_card", "payment_method_update", "succeeded", Some("pm_card"));
    f.post_event(&event("evt_card", "setup_intent.succeeded", json!({ "id": "seti_card" })))
        .await
        .assert_status_ok();
    assert_eq!(f.methods().await.stripe_payment_method_id.as_deref(), Some("pm_old"));
    // The browser call (which came second) applies it.
    assert_eq!(f.apply(MANAGER, "seti_card").await.json::<Value>()["status"], "saved");
}

#[test_db::test(seed)]
async fn a_failed_pending_bank_is_reported_and_a_card_decline_ignored(pool: MySqlPool) {
    let f = fixture(pool).await;
    // A card declined in the browser: never stored as pending, ignored.
    f.add_setup("seti_declined", "payment_method_update", "requires_payment_method", None);
    f.fake().fail_setup_intent("seti_declined", "card_declined");
    f.post_event(&event("evt_decl", "setup_intent.setup_failed", json!({ "id": "seti_declined" })))
        .await
        .assert_status_ok();
    assert!(f.emails_to(MANAGER_EMAIL).is_empty());

    f.add_setup("seti_bank", "payment_method_update", "requires_payment_method", None);
    f.fake().pend_microdeposits("seti_bank", "pm_bank");
    f.apply(MANAGER, "seti_bank").await.assert_status_ok();
    f.fake().fail_setup_intent("seti_bank", "payment_method_microdeposit_verification_timeout");
    let failed = event("evt_bank_fail", "setup_intent.setup_failed", json!({ "id": "seti_bank" }));
    f.post_event(&failed).await.assert_status_ok();
    f.post_event(&failed).await.assert_status_ok();

    let emails = f.emails_to(MANAGER_EMAIL);
    assert_eq!(subjects(&emails), ["We couldn't confirm your bank account"]);
    assert!(emails[0]["body"].as_str().unwrap().contains("Your current payment method stays in use."));
    let methods = f.methods().await;
    assert_eq!(methods.pending_setup_intent_id, None);
    assert_eq!(methods.stripe_payment_method_id.as_deref(), Some("pm_old"));
}

#[test_db::test(seed)]
async fn processing_is_marked_only_while_stripe_says_so(pool: MySqlPool) {
    let f = fixture(pool).await;
    let intent = |status: &str| {
        json!({ "id": "pi_1", "object": "payment_intent", "status": status, "customer": CUSTOMER,
            "payment_method_types": ["card", "us_bank_account"] })
    };
    // Out of order: it already succeeded when `processing` arrives.
    f.fake().payment_intents.insert("pi_1".into(), intent("succeeded"));
    f.post_event(&event("evt_proc_late", "payment_intent.processing", intent("processing")))
        .await
        .assert_status_ok();
    assert_eq!(f.methods().await.payment_processing_at, None);

    f.fake().payment_intents.insert("pi_1".into(), intent("processing"));
    f.post_event(&event("evt_proc", "payment_intent.processing", intent("processing")))
        .await
        .assert_status_ok();
    assert!(f.methods().await.payment_processing_at.is_some());
}

#[test_db::test(seed)]
async fn a_blocked_bank_account_holds_warns_then_pauses_until_a_new_method(pool: MySqlPool) {
    let f = fixture(pool).await;
    {
        let mut fake = f.fake();
        let mut blocked = bank_account("pm_old", "STRIPE TEST BANK", "6789");
        blocked["customer"] = json!(CUSTOMER);
        blocked["us_bank_account"]["status_details"] =
            json!({ "blocked": { "network_code": "R02", "reason": "bank_account_closed" } });
        fake.payment_methods.insert("pm_old".into(), blocked);
    }
    let updated = event(
        "evt_blocked",
        "payment_method.automatically_updated",
        json!({ "id": "pm_old", "object": "payment_method" }),
    );
    f.post_event(&updated).await.assert_status_ok();
    f.post_event(&updated).await.assert_status_ok();

    let methods = f.methods().await;
    assert_eq!(methods.method_hold.as_deref(), Some("blocked"));
    assert!(methods.method_hold_until.is_some());
    assert_eq!(
        subjects(&f.emails_to(MANAGER_EMAIL)),
        ["Your bank account can't be used anymore"]
    );
    let notices = f.emails_to(ALERT_EMAIL);
    assert_eq!(subjects(&notices), ["Billing alert: Bank account blocked: Pm Stone"]);
    let notice = notices[0]["body"].as_str().unwrap();
    assert!(notice.contains("bank_account_closed") && notice.contains("id 20750"));
    assert!(!notice.contains(MANAGER_EMAIL) && !notice.contains("6789"));

    let manager = f.gate(MANAGER).await;
    assert_eq!(manager["show_warning"], true);
    assert_eq!(manager["restricted"], false);
    assert_eq!(manager["reason"], "payment_method_required");
    assert_eq!(manager["days_remaining"], 7);
    let view: Value = f.server.get("/v1/billing/subscription").with_session(MANAGER).await.json();
    assert_eq!(view["payment_hold"]["reason"], "blocked");
    assert_eq!(view["payment_hold"]["paused"], false);

    // Seven days later: paused.
    f.sql("UPDATE company_billing SET method_hold_until = UTC_TIMESTAMP() - INTERVAL 1 MINUTE WHERE company_id = 20750")
        .await;
    let rep = f.gate(REP).await;
    assert_eq!(rep["restricted"], true);
    assert_eq!(rep["reason"], "payment_method_required");
    f.server
        .get("/v1/customers")
        .with_session(REP)
        .await
        .assert_status(StatusCode::PAYMENT_REQUIRED);

    // A new method ends the hold (billing routes stay open while paused).
    f.add_setup("seti_card", "payment_method_update", "succeeded", Some("pm_card"));
    assert_eq!(f.apply(MANAGER, "seti_card").await.json::<Value>()["status"], "saved");
    let methods = f.methods().await;
    assert_eq!(methods.method_hold, None);
    assert_eq!(methods.method_hold_until, None);
    assert_eq!(f.gate(REP).await["restricted"], false);
    f.server
        .get("/v1/customers")
        .with_session(REP)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn a_dispute_holds_the_company_and_tells_staff_only_when_configured(pool: MySqlPool) {
    let f = fixture_with(pool, false, stripe_signup()).await;
    f.fake().charges.insert(
        "ch_1".into(),
        json!({ "id": "ch_1", "object": "charge", "customer": CUSTOMER, "status": "succeeded",
            "payment_method": "pm_old", "amount": 37_500,
            "payment_method_details": { "type": "card" } }),
    );
    let dispute = json!({ "id": "dp_1", "object": "dispute", "charge": "ch_1",
        "reason": "fraudulent", "amount": 37_500 });
    f.post_event(&event("evt_dispute", "charge.dispute.created", dispute))
        .await
        .assert_status_ok();
    assert_eq!(f.methods().await.method_hold.as_deref(), Some("disputed"));
    let emails = f.emails_to(MANAGER_EMAIL);
    assert_eq!(subjects(&emails), ["A payment was disputed"]);
    assert!(emails[0]["body"].as_str().unwrap().contains("support@granite-manager.com"));
    // No BILLING_ALERT_EMAIL: no internal notice.
    assert!(f.emails_to(ALERT_EMAIL).is_empty());
    assert_eq!(f.sink.lock().unwrap().emails.len(), 1);
}

#[test_db::test(seed)]
async fn a_returned_bank_debit_sends_the_internal_notice(pool: MySqlPool) {
    let f = fixture(pool).await;
    {
        let mut fake = f.fake();
        fake.charges.insert(
            "ch_card".into(),
            json!({ "id": "ch_card", "object": "charge", "customer": CUSTOMER, "status": "failed",
                "failure_code": "card_declined", "amount": 37_500,
                "payment_method_details": { "type": "card" } }),
        );
        fake.charges.insert(
            "ch_ach".into(),
            json!({ "id": "ch_ach", "object": "charge", "customer": CUSTOMER, "status": "failed",
                "failure_code": "insufficient_funds", "amount": 37_500,
                "payment_method_details": { "type": "us_bank_account" } }),
        );
    }
    f.post_event(&event("evt_card_fail", "charge.failed", json!({ "id": "ch_card" })))
        .await
        .assert_status_ok();
    assert!(f.emails_to(ALERT_EMAIL).is_empty());
    f.post_event(&event("evt_ach_fail", "charge.failed", json!({ "id": "ch_ach" })))
        .await
        .assert_status_ok();
    let notices = f.emails_to(ALERT_EMAIL);
    assert_eq!(subjects(&notices), ["Billing alert: Bank debit returned: Pm Stone"]);
    let body = notices[0]["body"].as_str().unwrap();
    assert!(body.contains("insufficient_funds") && body.contains("$375.00"));
    // Managers hear about it from the dunning emails, not from this.
    assert!(f.emails_to(MANAGER_EMAIL).is_empty());
}

// ---- a signup waiting on its bank account (D1)

#[test_db::test(seed)]
async fn a_signup_waiting_on_its_bank_keeps_access_until_its_window_ends(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.sql(
        "UPDATE company_billing SET stripe_subscription_id = NULL, subscription_status = 'incomplete',
                payment_status = 'none', stripe_payment_method_id = NULL
          WHERE company_id = 20750;
         UPDATE company SET billing_required_at = UTC_TIMESTAMP() - INTERVAL 8 DAY WHERE id = 20750;",
    )
    .await;
    f.add_setup("seti_signup", "signup", "requires_payment_method", None);
    {
        let mut fake = f.fake();
        fake.setup_intents.get_mut("seti_signup").unwrap()["metadata"] =
            json!({ "signup_id": "5b120000-0000-4000-8000-000000020750", "purpose": "signup" });
        fake.pend_microdeposits("seti_signup", "pm_bank");
    }
    sqlx::query(
        "INSERT INTO signup_intents
           (id, status, email, company, owner_name, password_hash, plan, stripe_customer_id,
            setup_intent_id, company_id, bank_verification_until, created_at, updated_at, expires_at)
         VALUES ('5b120000-0000-4000-8000-000000020750', 'awaiting_bank_verification',
                 'manager@pm.test', ?, 'Dana Manager', '', ?, 'cus_pm', 'seti_signup', 20750,
                 UTC_TIMESTAMP() + INTERVAL 2 DAY, UTC_TIMESTAMP() - INTERVAL 8 DAY,
                 UTC_TIMESTAMP() - INTERVAL 8 DAY, UTC_TIMESTAMP() + INTERVAL 2 DAY)",
    )
    .bind(
        json!({ "name": "Pm Stone", "phone": "+13175550100", "website": null,
            "street": "1 Main St", "city": "Indianapolis", "state": "IN", "zip": "46204" })
        .to_string(),
    )
    .bind(json!({ "users": 3, "addons": [] }).to_string())
    .execute(&f.pool)
    .await
    .unwrap();

    // Eight days after payment was required, still inside the bank window.
    f.server.get("/v1/customers").with_session(REP).await.assert_status_ok();
    let gate = f.gate(MANAGER).await;
    assert_eq!(gate["restricted"], false);
    assert_eq!(gate["reason"], Value::Null);
    let view: Value = f.server.get("/v1/billing/subscription").with_session(MANAGER).await.json();
    assert_eq!(view["status"], "none");
    assert_eq!(view["plan_source"], "subscription");
    assert_eq!(view["pending_payment_method"]["status"], "verifying");
    assert_eq!(view["pending_payment_method"]["last4"], "6789");
    assert_eq!(view["payment_hold"], Value::Null);

    // The window ends: paused until a payment method is added.
    f.sql(
        "UPDATE signup_intents SET bank_verification_until = UTC_TIMESTAMP() - INTERVAL 1 MINUTE
          WHERE company_id = 20750",
    )
    .await;
    f.server
        .get("/v1/customers")
        .with_session(REP)
        .await
        .assert_status(StatusCode::PAYMENT_REQUIRED);
    let gate = f.gate(MANAGER).await;
    assert_eq!(gate["restricted"], true);
    assert_eq!(gate["reason"], "payment_method_required");
    let view: Value = f.server.get("/v1/billing/subscription").with_session(MANAGER).await.json();
    assert_eq!(view["payment_hold"]["reason"], "bank_unverified");
    assert_eq!(view["payment_hold"]["paused"], true);

    // The bank verification fails: the Managers are told to add a method.
    f.fake().fail_setup_intent("seti_signup", "payment_method_microdeposit_failed");
    f.post_event(&event("evt_signup_fail", "setup_intent.setup_failed", json!({ "id": "seti_signup" })))
        .await
        .assert_status_ok();
    let emails = f.emails_to(MANAGER_EMAIL);
    assert_eq!(subjects(&emails), ["We couldn't confirm your bank account"]);
    assert!(emails[0]["body"].as_str().unwrap().contains("Add a card or another bank account by"));
}

// ---- D14

#[test_db::test(seed)]
async fn no_request_sends_a_statement_descriptor(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.past_due().await;
    f.fake().invoices = vec![invoice("in_1", 100)];
    f.server
        .post("/v1/billing/payment-setup")
        .with_session(MANAGER)
        .json(&json!({ "purpose": "update" }))
        .await
        .assert_status_ok();
    f.add_setup("seti_card", "payment_method_update", "succeeded", Some("pm_card"));
    f.apply(MANAGER, "seti_card").await.assert_status_ok();
    f.pay(UUID).await.assert_status_ok();
    let fake = f.fake();
    assert!(fake.requests.len() > 4);
    for seen in &fake.requests {
        assert!(
            !seen.fields().iter().any(|(key, _)| key.contains("statement_descriptor")),
            "{} {} sent a statement descriptor",
            seen.method,
            seen.path
        );
    }
}

#[test_db::test(seed)]
async fn the_other_company_is_never_touched(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.add_setup("seti_card", "payment_method_update", "succeeded", Some("pm_card"));
    f.apply(MANAGER, "seti_card").await.assert_status_ok();
    let other: Option<String> = sqlx::query_scalar(
        "SELECT stripe_payment_method_id FROM company_billing WHERE company_id = ?",
    )
    .bind(OTHER_COMPANY)
    .fetch_optional(&f.pool)
    .await
    .unwrap()
    .flatten();
    assert_eq!(other, None);
}
