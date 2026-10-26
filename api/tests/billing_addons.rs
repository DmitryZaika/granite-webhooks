//! The payments app's account page: `GET /v1/billing/subscription` and
//! `PUT /v1/billing/addons/{key}`, against the seeded Sb10 companies and a
//! fake Stripe.

mod common;

use api::app;
use api::billing::lock::CompanyLock;
use api::billing::stripe::signature_header;
use api::state::AppState;
use axum::http::{Method, StatusCode};
use axum_test::{TestResponse, TestServer};
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use common::{ALLOWED_ORIGIN, SECRET, WithSession};
use fake_stripe::{FailMode, FakeStripe, Shared, bank_account, card, price};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::future::IntoFuture;
use std::time::Duration;

#[path = "support/fake_stripe.rs"]
mod fake_stripe;

/// Seeded: Sb10 Stone, its Manager and rep; Sb10 Other Stone and its Manager.
const COMPANY: i32 = 20500;
const OTHER_COMPANY: i32 = 20501;
const MANAGER: &str = "aaaaaaaa-0000-4000-8000-000000020500";
const REP: &str = "aaaaaaaa-0000-4000-8000-000000020501";
const OTHER_MANAGER: &str = "aaaaaaaa-0000-4000-8000-000000020502";
const CUSTOMER: &str = "cus_sb10";
const SUBSCRIPTION: &str = "sub_sb10";
const OTHER_CUSTOMER: &str = "cus_sb10_other";
const OTHER_SUBSCRIPTION: &str = "sub_sb10_other";
/// 2037-01-21 05:00:00 UTC, the seeded `trial_end_at`.
const TRIAL_END: i64 = 2_116_126_800;
const TRIAL_END_ISO: &str = "2037-01-21T05:00:00Z";
const ITEMS: &str = "/v1/subscription_items";
const VIEW: &str = "/v1/billing/subscription";
const DAY: i64 = 86_400;
const WEBHOOK_SECRET: &str = "whsec_sb10_test";

struct Fixture {
    pool: MySqlPool,
    server: TestServer,
    stripe: Shared,
    /// The current period of every item: 10 days ago to 20 days ahead.
    period_start: i64,
    period_end: i64,
}

fn item(id: &str, price: Value, start: i64, end: i64) -> Value {
    json!({
        "id": id, "object": "subscription_item", "quantity": 1, "price": price,
        "current_period_start": start, "current_period_end": end,
    })
}

fn base_price() -> Value {
    price("price_base", "granite_base_monthly_v1", 30_000)
}

fn quotes_price() -> Value {
    price("price_quotes", "addon_quotes_monthly_v1", 7_500)
}

fn invoice(id: &str, customer: &str, number: &str, status: &str, created: i64) -> Value {
    json!({
        "id": id, "object": "invoice", "customer": customer, "number": number,
        "status": status, "amount_due": 37_500,
        "amount_paid": if status == "paid" { 37_500 } else { 0 },
        "created": created, "status_transitions": { "paid_at": created + 60 },
        "hosted_invoice_url": format!("https://pay.stripe.test/{id}"), "invoice_pdf": null,
    })
}

fn iso(unix: i64) -> String {
    DateTime::<Utc>::from_timestamp(unix, 0)
        .unwrap()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The seeded company 20500 with its Stripe side: a trialing subscription
/// with the base and Quotes items, charged to a Visa card, and its invoices.
async fn fixture(pool: MySqlPool) -> Fixture {
    let now = Utc::now().timestamp();
    let (period_start, period_end) = (now - 10 * DAY, now + 20 * DAY);
    let stripe = FakeStripe::with_catalog();
    {
        let mut fake = stripe.lock().unwrap();
        fake.preview_tax = 1_234;
        fake.subscriptions.insert(
            SUBSCRIPTION.into(),
            json!({
                "id": SUBSCRIPTION, "object": "subscription", "customer": CUSTOMER,
                "status": "trialing", "trial_end": TRIAL_END,
                "default_payment_method": "pm_card",
                "metadata": { "company_id": COMPANY.to_string() },
                "items": { "object": "list", "data": [
                    item("si_sb10_base", base_price(), period_start, period_end),
                    item("si_sb10_quotes", quotes_price(), period_start, period_end),
                ] },
            }),
        );
        fake.customers.insert(
            CUSTOMER.into(),
            json!({ "id": CUSTOMER, "object": "customer", "metadata": {},
                "invoice_settings": { "default_payment_method": null } }),
        );
        fake.payment_methods
            .insert("pm_card".into(), card("pm_card", "visa", "4242"));
        fake.payment_methods.insert(
            "pm_bank".into(),
            bank_account("pm_bank", "STRIPE TEST BANK", "6789"),
        );
        fake.invoices = vec![
            invoice("in_sb10_draft", CUSTOMER, "GM-0003", "draft", now),
            invoice("in_sb10_2", CUSTOMER, "GM-0002", "open", now - DAY),
            invoice("in_sb10_1", CUSTOMER, "GM-0001", "paid", now - 30 * DAY),
        ];
    }
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_stripe(fake_stripe::client(&stripe).await)
        .with_stripe_webhook_secret(WEBHOOK_SECRET);
    let server = TestServer::new(app(state));
    Fixture {
        pool,
        server,
        stripe,
        period_start,
        period_end,
    }
}

impl Fixture {
    async fn get(&self, session: &str) -> TestResponse {
        self.server.get(VIEW).with_session(session).await
    }

    async fn view(&self) -> Value {
        let res = self.get(MANAGER).await;
        res.assert_status_ok();
        res.json()
    }

    async fn put(&self, session: &str, key: &str, enabled: bool) -> TestResponse {
        self.server
            .put(&format!("/v1/billing/addons/{key}"))
            .with_session(session)
            .json(&json!({ "enabled": enabled }))
            .await
    }

    async fn sql(&self, statement: &'static str) {
        sqlx::raw_sql(statement).execute(&self.pool).await.unwrap();
    }

    fn calls(&self, method: &str, path: &str) -> Vec<fake_stripe::Seen> {
        self.stripe.lock().unwrap().calls(method, path)
    }

    /// Writes to subscription items (adds and removes).
    fn item_writes(&self) -> usize {
        self.stripe
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|seen| seen.method != "GET" && seen.path.starts_with(ITEMS))
            .count()
    }

    fn set_subscription(&self, id: &str, field: &str, value: Value) {
        self.stripe
            .lock()
            .unwrap()
            .subscriptions
            .get_mut(id)
            .unwrap()[field] = value;
    }

    fn live_item_ids(&self, subscription: &str) -> Vec<(String, String)> {
        self.stripe.lock().unwrap().subscriptions[subscription]["items"]["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["id"].as_str().unwrap().to_string(),
                    item["price"]["lookup_key"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// (item id, ended_at) of the add-on's subscription row.
    async fn addon_row(
        &self,
        company_id: i32,
        key: &str,
    ) -> Option<(Option<String>, Option<NaiveDateTime>)> {
        sqlx::query_as(
            "SELECT stripe_subscription_item_id, ended_at FROM company_addons
              WHERE company_id = ? AND addon_key = ? AND source = 'subscription'",
        )
        .bind(company_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .unwrap()
    }

    /// 10 more sales reps: 12 billable users.
    async fn add_reps(&self) {
        for id in 20_510..20_520 {
            sqlx::query(
                "INSERT INTO users (id, email, password, name, is_employee, company_id)
                 VALUES (?, ?, 'x', 'Sb10 Rep', 1, ?)",
            )
            .bind(id)
            .bind(format!("rep{id}@sb10.test"))
            .bind(COMPANY)
            .execute(&self.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (?, 1, ?)",
            )
            .bind(id)
            .bind(COMPANY)
            .execute(&self.pool)
            .await
            .unwrap();
        }
    }

    async fn grant_comp(&self, key: &str) {
        sqlx::query(
            "INSERT INTO company_addons (company_id, addon_key, source, granted_by, started_at)
             VALUES (?, ?, 'comp', NULL, '2026-10-01 00:00:00')",
        )
        .bind(COMPANY)
        .bind(key)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// Waits until a connection of this test's database waits in `GET_LOCK`.
    async fn wait_for_lock_waiter(&self) {
        for _ in 0..250 {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM information_schema.PROCESSLIST
                  WHERE DB = DATABASE() AND STATE = 'User lock'",
            )
            .fetch_one(&self.pool)
            .await
            .unwrap();
            if waiting > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("nothing waited for the company lock");
    }

    /// The subscription out of its free period, in a paid month.
    async fn make_active(&self) {
        self.set_subscription(SUBSCRIPTION, "status", json!("active"));
        self.set_subscription(SUBSCRIPTION, "trial_end", Value::Null);
        self.sql(
            "UPDATE company_billing SET subscription_status = 'active' WHERE company_id = 20500",
        )
        .await;
    }
}

fn addon<'a>(view: &'a Value, key: &str) -> &'a Value {
    view["addons"]
        .as_array()
        .unwrap()
        .iter()
        .find(|addon| addon["key"] == key)
        .unwrap()
}

#[test_db::test(seed)]
async fn a_trialing_company_sees_its_plan_users_addons_and_next_invoice(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.add_reps().await;
    f.grant_comp("signatures").await;

    let view = f.view().await;
    assert_eq!(view["company_name"], "Sb10 Stone");
    assert_eq!(view["plan_source"], "subscription");
    assert_eq!(view["status"], "trialing");
    assert_eq!(view["payment_status"], "active");
    assert_eq!(view["trial_end_at"], TRIAL_END_ISO);
    assert_eq!(view["next_charge_at"], TRIAL_END_ISO);
    assert_eq!(view["current_period_end"], iso(f.period_end));
    assert_eq!(
        view["base"],
        json!({ "label": "Base plan", "monthly_cents": 30_000, "included_users": 10 })
    );
    assert_eq!(
        view["users"],
        json!({ "billable": 12, "included": 10, "extra": 2, "extra_unit_cents": 3_000,
                "extra_monthly_cents": 6_000 })
    );
    assert_eq!(
        view["addons"],
        json!([
            { "key": "signatures", "label": "Signatures",
              "description": "eSignatures and an email signature block",
              "monthly_cents": 1_500, "status": "included", "prorated_cents": 0 },
            { "key": "quotes", "label": "Quotes",
              "description": "Quote editor, price lists and online quotes",
              "monthly_cents": 7_500, "status": "active", "prorated_cents": 0 },
            { "key": "quickbooks", "label": "QuickBooks",
              "description": "Invoices and QuickBooks sync",
              "monthly_cents": 3_000, "status": "available", "prorated_cents": 0 },
        ])
    );
    // Stripe's items: base + Quotes (the extra users are pushed by seat sync).
    assert_eq!(view["monthly_total_cents"], 37_500);
    assert_eq!(
        view["next_invoice"],
        json!({
            "date": TRIAL_END_ISO, "subtotal_cents": 37_500, "tax_cents": 1_234,
            "total_cents": 38_734, "amount_due_cents": 38_734,
            "lines": [
                { "description": "1 x granite_base_monthly_v1", "amount_cents": 30_000 },
                { "description": "1 x addon_quotes_monthly_v1", "amount_cents": 7_500 },
            ],
        })
    );
    assert_eq!(
        view["payment_method"],
        json!({ "type": "card", "brand": "visa", "last4": "4242", "exp_month": 12,
                "exp_year": 2030, "bank_name": null })
    );
    assert_eq!(view["payment_method_known"], true);
    assert_eq!(view["cancellation"], Value::Null);
    assert_eq!(view["can_change"], true);
    assert_eq!(view["can_cancel"], true);
    // Newest first, drafts hidden, the shape of `GET /v1/billing`.
    let invoices = view["invoices"].as_array().unwrap();
    assert_eq!(invoices.len(), 2);
    assert_eq!(invoices[0]["number"], "GM-0002");
    assert_eq!(invoices[0]["status"], "open");
    assert_eq!(invoices[0]["amount_due_cents"], 37_500);
    assert_eq!(invoices[1]["number"], "GM-0001");
    assert_eq!(invoices[1]["amount_due_cents"], 0);
    assert_eq!(invoices[1]["amount_paid_cents"], 37_500);
    // The preview is keyed (a POST), never replayed.
    let previews = f.calls("POST", "/v1/invoices/create_preview");
    assert_eq!(previews.len(), 1);
    assert!(previews[0].idempotency_key.is_some());
    assert_eq!(
        previews[0].field("subscription").as_deref(),
        Some(SUBSCRIPTION)
    );
}

#[test_db::test(seed)]
async fn a_bank_account_on_the_customer_is_summarized(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.set_subscription(SUBSCRIPTION, "default_payment_method", Value::Null);
    f.stripe
        .lock()
        .unwrap()
        .customers
        .get_mut(CUSTOMER)
        .unwrap()["invoice_settings"]["default_payment_method"] = json!("pm_bank");

    let view = f.view().await;
    assert_eq!(
        view["payment_method"],
        json!({ "type": "us_bank_account", "brand": null, "last4": "6789", "exp_month": null,
                "exp_year": null, "bank_name": "STRIPE TEST BANK" })
    );
}

#[test_db::test(seed)]
async fn a_customer_without_a_payment_method_is_known_to_have_none(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.set_subscription(SUBSCRIPTION, "default_payment_method", Value::Null);

    let view = f.view().await;
    assert_eq!(view["payment_method"], Value::Null);
    assert_eq!(view["payment_method_known"], true);
}

#[test_db::test(seed)]
async fn a_payment_method_stripe_cannot_read_is_unknown_not_missing(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.set_subscription(SUBSCRIPTION, "default_payment_method", Value::Null);
    f.stripe.lock().unwrap().fail(
        Method::GET,
        &format!("/v1/customers/{CUSTOMER}"),
        FailMode::Before,
        1,
    );

    let view = f.view().await;
    assert_eq!(view["payment_method"], Value::Null);
    assert_eq!(view["payment_method_known"], false);
    // The rest of the page still came from Stripe.
    assert_eq!(view["status"], "trialing");
    assert_ne!(view["next_invoice"], Value::Null);
}

#[test_db::test(seed)]
async fn a_scheduled_cancellation_shows_and_clears_the_next_charge(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.sql(
        "INSERT INTO billing_cancellation_requests
           (id, company_id, requested_by, reason, effective_at, status, requested_at)
         VALUES (7, 20500, 20500, 'Too busy', '2037-01-21 05:00:00', 'scheduled',
                 '2026-10-20 15:00:00'),
                (8, 20500, 20500, NULL, '2037-01-21 05:00:00', 'withdrawn',
                 '2026-10-21 15:00:00')",
    )
    .await;

    let view = f.view().await;
    assert_eq!(
        view["cancellation"],
        json!({ "id": 7, "requested_at": "2026-10-20T15:00:00Z",
                "effective_at": TRIAL_END_ISO, "requested_by": "Dana Manager",
                "reason": "Too busy" })
    );
    assert_eq!(view["next_charge_at"], Value::Null);
    assert_eq!(view["status"], "trialing");
}

#[test_db::test(seed)]
async fn a_legacy_company_gets_only_its_plan_source(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.sql("UPDATE company SET plan_source = 'legacy' WHERE id = 20500")
        .await;

    let view = f.view().await;
    assert_eq!(
        view,
        json!({
            "company_name": "Sb10 Stone", "plan_source": "legacy", "status": "none",
            "payment_status": "none", "trial_end_at": null, "current_period_end": null,
            "next_charge_at": null, "base": null, "users": null, "addons": [],
            "monthly_total_cents": null, "next_invoice": null, "payment_method": null,
            "payment_method_known": false, "cancellation": null, "invoices": [], "can_change": false,
            "can_cancel": false,
        })
    );
    assert!(f.stripe.lock().unwrap().requests.is_empty());
}

#[test_db::test(seed)]
async fn the_page_falls_back_to_the_stored_rows_when_stripe_fails(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.stripe
        .lock()
        .unwrap()
        .fail(Method::GET, "/v1/subscriptions/", FailMode::Before, 1);

    let view = f.view().await;
    assert_eq!(view["status"], "trialing");
    assert_eq!(view["trial_end_at"], TRIAL_END_ISO);
    assert_eq!(view["next_charge_at"], TRIAL_END_ISO);
    assert_eq!(view["monthly_total_cents"], 37_500);
    assert_eq!(addon(&view, "quotes")["status"], "active");
    assert_eq!(addon(&view, "quickbooks")["status"], "available");
    assert_eq!(view["base"]["monthly_cents"], 30_000);
    assert_eq!(view["next_invoice"], Value::Null);
    assert!(f.calls("POST", "/v1/invoices/create_preview").is_empty());
}

#[test_db::test(seed)]
async fn an_unreachable_stripe_skips_the_reads_that_need_the_subscription(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.stripe
        .lock()
        .unwrap()
        .fail(Method::GET, "/v1/subscriptions/", FailMode::Before, 1);

    let view = f.view().await;
    assert_eq!(view["status"], "trialing");
    assert_eq!(view["monthly_total_cents"], 37_500);
    assert_eq!(addon(&view, "quotes")["status"], "active");
    assert_eq!(view["next_invoice"], Value::Null);
    assert_eq!(view["payment_method"], Value::Null);
    // Unknown, not "none on file".
    assert_eq!(view["payment_method_known"], false);
    assert_eq!(view["can_cancel"], true);
    // The prices and invoices were read alongside the subscription; nothing
    // was asked after its outage (no preview, payment method or customer).
    let requests = f.stripe.lock().unwrap().requests.clone();
    assert_eq!(
        f.calls("GET", &format!("/v1/subscriptions/{SUBSCRIPTION}"))
            .len(),
        1
    );
    assert!(
        requests.iter().all(|seen| seen.method == "GET"
            && [
                format!("/v1/subscriptions/{SUBSCRIPTION}").as_str(),
                "/v1/prices",
                "/v1/invoices",
            ]
            .contains(&seen.path.as_str())),
        "{requests:?}"
    );
}

#[test_db::test(seed)]
async fn without_prices_the_subscription_can_still_be_cancelled(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.stripe
        .lock()
        .unwrap()
        .fail(Method::GET, "/v1/prices", FailMode::Before, 1);

    let view = f.view().await;
    assert_eq!(view["status"], "trialing");
    assert_eq!(view["can_change"], false);
    assert_eq!(view["can_cancel"], true);
}

#[test_db::test(seed)]
async fn an_ended_subscription_cannot_be_cancelled(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.set_subscription(SUBSCRIPTION, "status", json!("canceled"));
    f.sql("UPDATE company_billing SET subscription_status = 'canceled' WHERE company_id = 20500")
        .await;

    let view = f.view().await;
    assert_eq!(view["status"], "canceled");
    assert_eq!(view["can_change"], false);
    assert_eq!(view["can_cancel"], false);
}

#[test_db::test(seed)]
async fn an_active_company_sees_prorated_estimates(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.make_active().await;

    let view = f.view().await;
    assert_eq!(view["status"], "active");
    assert_eq!(view["next_charge_at"], iso(f.period_end));
    // 20 of 30 days left: two thirds of the monthly price.
    let quickbooks = addon(&view, "quickbooks")["prorated_cents"]
        .as_i64()
        .unwrap();
    assert!((1_990..=2_000).contains(&quickbooks), "{quickbooks}");
    let quotes = addon(&view, "quotes")["prorated_cents"].as_i64().unwrap();
    assert!((4_990..=5_000).contains(&quotes), "{quotes}");
}

#[test_db::test(seed)]
async fn enabling_an_addon_adds_a_prorated_item_and_its_row(pool: MySqlPool) {
    let f = fixture(pool).await;

    let res = f.put(MANAGER, "quickbooks", true).await;
    res.assert_status_ok();
    let view: Value = res.json();
    assert_eq!(addon(&view, "quickbooks")["status"], "active");

    let added = f.calls("POST", ITEMS);
    assert_eq!(added.len(), 1);
    assert_eq!(
        added[0].field("subscription").as_deref(),
        Some(SUBSCRIPTION)
    );
    assert_eq!(added[0].field("price").as_deref(), Some("price_quickbooks"));
    assert_eq!(added[0].field("quantity").as_deref(), Some("1"));
    assert_eq!(
        added[0].field("proration_behavior").as_deref(),
        Some("create_prorations")
    );
    assert_eq!(
        added[0].idempotency_key.as_deref(),
        Some("addon:20500:quickbooks:first")
    );
    let item_id = f
        .live_item_ids(SUBSCRIPTION)
        .into_iter()
        .find(|(_, key)| key == "addon_quickbooks_monthly_v1")
        .map(|(id, _)| id)
        .unwrap();
    // Written at once, before any webhook.
    assert_eq!(
        f.addon_row(COMPANY, "quickbooks").await,
        Some((Some(item_id), None))
    );
}

#[test_db::test(seed)]
async fn disabling_an_addon_deletes_its_item_and_ends_its_row(pool: MySqlPool) {
    let f = fixture(pool).await;

    let res = f.put(MANAGER, "quotes", false).await;
    res.assert_status_ok();
    let view: Value = res.json();
    assert_eq!(addon(&view, "quotes")["status"], "available");
    let removed = f.calls("DELETE", "/v1/subscription_items/si_sb10_quotes");
    assert_eq!(removed.len(), 1);
    assert_eq!(
        removed[0].field("proration_behavior").as_deref(),
        Some("create_prorations")
    );
    let (item, ended) = f.addon_row(COMPANY, "quotes").await.unwrap();
    assert_eq!(item.as_deref(), Some("si_sb10_quotes"));
    let ended = ended.expect("the row ended");

    // Added again later: a new item under a new key (not a replay).
    let res = f.put(MANAGER, "quotes", true).await;
    res.assert_status_ok();
    let added = f.calls("POST", ITEMS);
    assert_eq!(added.len(), 1);
    assert_eq!(
        added[0].idempotency_key,
        Some(format!(
            "addon:20500:quotes:{}",
            ended.and_utc().timestamp()
        ))
    );
    let (item, ended) = f.addon_row(COMPANY, "quotes").await.unwrap();
    assert_ne!(item.as_deref(), Some("si_sb10_quotes"));
    assert_eq!(ended, None);
}

#[test_db::test(seed)]
async fn asking_for_the_current_state_sends_nothing_to_stripe(pool: MySqlPool) {
    let f = fixture(pool).await;

    let res = f.put(MANAGER, "quotes", true).await;
    res.assert_status_ok();
    assert_eq!(addon(&res.json::<Value>(), "quotes")["status"], "active");
    f.put(MANAGER, "quickbooks", false).await.assert_status_ok();
    assert_eq!(f.item_writes(), 0);
    assert_eq!(
        f.addon_row(COMPANY, "quotes").await,
        Some((Some("si_sb10_quotes".into()), None))
    );
    assert_eq!(f.addon_row(COMPANY, "quickbooks").await, None);
}

#[test_db::test(seed)]
async fn adding_waits_for_an_overdue_payment_but_removing_does_not(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.set_subscription(SUBSCRIPTION, "status", json!("past_due"));
    f.sql(
        "UPDATE company_billing
            SET subscription_status = 'past_due', payment_status = 'past_due',
                failed_payment_at = UTC_TIMESTAMP(),
                grace_period_ends_at = UTC_TIMESTAMP() + INTERVAL 7 DAY
          WHERE company_id = 20500",
    )
    .await;

    let res = f.put(MANAGER, "quickbooks", true).await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        res.json::<Value>()["error"],
        "Pay the open invoice first, then add add-ons."
    );
    assert_eq!(f.item_writes(), 0);

    f.put(MANAGER, "quotes", false).await.assert_status_ok();
    assert_eq!(
        f.calls("DELETE", "/v1/subscription_items/si_sb10_quotes")
            .len(),
        1
    );
}

#[test_db::test(seed)]
async fn an_included_addon_cannot_be_changed_here(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.grant_comp("signatures").await;

    for enabled in [true, false] {
        let res = f.put(MANAGER, "signatures", enabled).await;
        res.assert_status(StatusCode::CONFLICT);
        let error = res.json::<Value>()["error"].as_str().unwrap().to_string();
        assert!(error.contains("included free"), "{error}");
        assert!(error.contains("support@granite-manager.com"), "{error}");
    }
    assert_eq!(f.item_writes(), 0);
}

#[test_db::test(seed)]
async fn a_legacy_company_cannot_change_addons(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.sql("UPDATE company SET plan_source = 'legacy' WHERE id = 20500")
        .await;

    f.put(MANAGER, "quickbooks", true)
        .await
        .assert_status(StatusCode::CONFLICT);
    assert!(f.stripe.lock().unwrap().requests.is_empty());
}

#[test_db::test(seed)]
async fn an_ended_subscription_cannot_change_addons(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.set_subscription(SUBSCRIPTION, "status", json!("canceled"));

    f.put(MANAGER, "quickbooks", true)
        .await
        .assert_status(StatusCode::CONFLICT);
    assert_eq!(f.item_writes(), 0);
}

#[test_db::test(seed)]
async fn an_unknown_addon_is_not_found(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.put(MANAGER, "telepathy", true)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert_eq!(f.item_writes(), 0);
}

#[test_db::test(seed)]
async fn only_managers_see_or_change_the_subscription(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.get(REP).await.assert_status(StatusCode::FORBIDDEN);
    f.put(REP, "quickbooks", true)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    f.server
        .get(VIEW)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(f.item_writes(), 0);
}

#[test_db::test(seed)]
async fn another_companys_manager_only_touches_their_own_subscription(pool: MySqlPool) {
    let f = fixture(pool).await;
    {
        let mut fake = f.stripe.lock().unwrap();
        fake.subscriptions.insert(
            OTHER_SUBSCRIPTION.into(),
            json!({
                "id": OTHER_SUBSCRIPTION, "object": "subscription", "customer": OTHER_CUSTOMER,
                "status": "active", "metadata": { "company_id": OTHER_COMPANY.to_string() },
                "items": { "object": "list", "data": [
                    item("si_other_base", base_price(), f.period_start, f.period_end),
                ] },
            }),
        );
        fake.invoices.push(invoice(
            "in_other_1",
            OTHER_CUSTOMER,
            "OT-0001",
            "paid",
            f.period_start,
        ));
    }
    f.sql(
        "INSERT INTO company_billing
           (company_id, stripe_customer_id, stripe_subscription_id, subscription_status,
            payment_status)
         VALUES (20501, 'cus_sb10_other', 'sub_sb10_other', 'active', 'active')",
    )
    .await;

    let view = {
        let res = f.get(OTHER_MANAGER).await;
        res.assert_status_ok();
        res.json::<Value>()
    };
    assert_eq!(view["company_name"], "Sb10 Other Stone");
    assert_eq!(addon(&view, "quotes")["status"], "available");
    assert_eq!(view["users"]["billable"], 1);
    let invoices = view["invoices"].as_array().unwrap();
    assert_eq!(invoices.len(), 1);
    assert_eq!(invoices[0]["number"], "OT-0001");

    f.put(OTHER_MANAGER, "quotes", true)
        .await
        .assert_status_ok();
    let added = f.calls("POST", ITEMS);
    assert_eq!(added.len(), 1);
    assert_eq!(
        added[0].field("subscription").as_deref(),
        Some(OTHER_SUBSCRIPTION)
    );
    assert_eq!(
        added[0].idempotency_key.as_deref(),
        Some("addon:20501:quotes:first")
    );
    f.put(OTHER_MANAGER, "quotes", false)
        .await
        .assert_status_ok();

    // Nothing the other Manager did read or wrote company 20500's Stripe side.
    let requests = f.stripe.lock().unwrap().requests.clone();
    assert!(requests.iter().all(|seen| {
        !seen.path.ends_with(&format!("/{SUBSCRIPTION}"))
            && !seen.path.ends_with(&format!("/{CUSTOMER}"))
            && !seen.path.ends_with("/si_sb10_quotes")
            && seen.field("subscription").as_deref() != Some(SUBSCRIPTION)
            && seen.field("customer").as_deref() != Some(CUSTOMER)
    }));
    assert_eq!(f.live_item_ids(SUBSCRIPTION).len(), 2);
    assert_eq!(
        f.addon_row(COMPANY, "quotes").await,
        Some((Some("si_sb10_quotes".into()), None))
    );
    assert_eq!(f.addon_row(COMPANY, "quickbooks").await, None);
}

#[test_db::test(seed)]
async fn a_webhook_reads_stripe_only_after_taking_the_company_lock(pool: MySqlPool) {
    let f = fixture(pool).await;
    let path = format!("/v1/subscriptions/{SUBSCRIPTION}");
    let lock = CompanyLock::acquire(&f.pool, COMPANY).await.unwrap();
    // The event carries the subscription as it was: base + Quotes.
    let object = f.stripe.lock().unwrap().subscriptions[SUBSCRIPTION].clone();
    let payload = serde_json::to_vec(&json!({
        "id": "evt_sb10_lock", "type": "customer.subscription.updated",
        "data": { "object": object },
    }))
    .unwrap();
    let header = signature_header(&payload, WEBHOOK_SECRET, Utc::now().timestamp());
    let webhook = f
        .server
        .post("/v1/webhooks/stripe")
        .add_header("stripe-signature", header)
        .add_header("content-type", "application/json")
        .bytes(payload.into());
    let meanwhile = async {
        f.wait_for_lock_waiter().await;
        assert!(
            f.calls("GET", &path).is_empty(),
            "Stripe read before the lock"
        );
        // Under the lock, Quotes is swapped for QuickBooks.
        f.set_subscription(
            SUBSCRIPTION,
            "items",
            json!({ "object": "list", "data": [
                item("si_sb10_base", base_price(), f.period_start, f.period_end),
                item(
                    "si_sb10_quickbooks",
                    price("price_quickbooks", "addon_quickbooks_monthly_v1", 3_000),
                    f.period_start,
                    f.period_end,
                ),
            ] }),
        );
        lock.release().await;
    };
    let (res, ()) = tokio::join!(webhook.into_future(), meanwhile);
    res.assert_status_ok();

    assert_eq!(f.calls("GET", &path).len(), 1);
    assert_eq!(
        f.addon_row(COMPANY, "quickbooks").await,
        Some((Some("si_sb10_quickbooks".into()), None))
    );
    let (item, ended) = f.addon_row(COMPANY, "quotes").await.unwrap();
    assert_eq!(item.as_deref(), Some("si_sb10_quotes"));
    assert!(ended.is_some(), "Quotes left the subscription");
}
