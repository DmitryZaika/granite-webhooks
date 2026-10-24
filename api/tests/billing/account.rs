//! Ports of the account tests (`billingCheckout`, `billingSync`, `billingReactivation`, `billingPageData`).

use super::*;
use api::billing::account::{Reconciled, reconcile_batch, reconcile_with_stripe};
use api::billing::lock::CompanyLock;

/// Nothing listens there: requests fail at once.
const UNREACHABLE: &str = "http://127.0.0.1:9";

/// The forms (or queries) of the fake Stripe's `method path` requests.
fn sent(stripe: &Shared, method: &str, path: &str) -> Vec<HashMap<String, String>> {
    stripe
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.0 == method && r.1 == path)
        .map(|r| form(&r.2))
        .collect()
}

#[test_db::test(seed)]
async fn the_portal_reuses_the_tagged_no_cancel_configuration(pool: MySqlPool) {
    let stripe = Shared::default();
    manager_and_billing(&pool).await;
    let server = server(&pool, Some(&stripe)).await;
    let none = server
        .post("/v1/billing/portal")
        .with_session(sessions::MANAGER)
        .json(&json!({}))
        .await;
    none.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        none.json::<Value>(),
        json!({ "error": "Subscribe to a plan first." })
    );

    subscribed_row(&pool).await;
    stripe.lock().unwrap().portal_configurations = vec![
        json!({ "id": "bpc_default", "active": true, "metadata": {} }),
        json!({ "id": "bpc_tagged", "active": true, "metadata": { "app": "granite-manager-no-cancel-v1" } }),
    ];
    server
        .post("/v1/billing/portal")
        .with_session(sessions::MANAGER)
        .json(&json!({}))
        .await
        .assert_status_ok();
    let listed = sent(&stripe, "GET", "/v1/billing_portal/configurations");
    assert_eq!(listed[0]["active"], "true");
    assert_eq!(listed[0]["limit"], "100");
    assert!(sent(&stripe, "POST", "/v1/billing_portal/configurations").is_empty());
    assert_eq!(
        sent(&stripe, "POST", "/v1/billing_portal/sessions")[0]["configuration"],
        "bpc_tagged"
    );
}

/// Company 100 has a customer and only an ended subscription.
async fn subscribed_row(pool: &MySqlPool) {
    sqlx::query(
        "INSERT INTO company_billing (company_id, stripe_customer_id, stripe_subscription_id,
                                      subscription_status, payment_status)
         VALUES (100, 'cus_100', 'sub_old', 'canceled', 'none')",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn checkout_waits_while_stripe_already_has_a_live_subscription(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 5, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    subscribed_row(&pool).await;
    let stripe = Shared::default();
    // Checkout just finished; its webhook has not reached us yet.
    stripe.lock().unwrap().subscriptions.insert(
        "sub_live".into(),
        json!({ "id": "sub_live", "object": "subscription", "status": "active", "customer": "cus_100",
                "items": { "data": [] } }),
    );
    let server = server(&pool, Some(&stripe)).await;
    let refused = server
        .post("/v1/billing/checkout")
        .with_session(sessions::MANAGER)
        .json(&json!({ "price_id": "price_flat" }))
        .await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>(),
        json!({ "error": "This company already has a subscription. Update the plan instead." })
    );
    assert!(sent(&stripe, "POST", "/v1/checkout/sessions").is_empty());

    // An abandoned first payment does not count.
    stripe
        .lock()
        .unwrap()
        .subscriptions
        .get_mut("sub_live")
        .unwrap()["status"] = json!("incomplete");
    let link: Value = server
        .post("/v1/billing/checkout")
        .with_session(sessions::MANAGER)
        .json(&json!({ "price_id": "price_flat" }))
        .await
        .json();
    assert_eq!(link["url"], "https://checkout.stripe.com/c/cs_1");
    assert_eq!(
        sent(&stripe, "POST", "/v1/checkout/sessions")[0]["customer"],
        "cus_100"
    );
}

#[test_db::test(seed)]
async fn plans_are_priced_by_stripe_with_the_configured_cents_as_fallback(pool: MySqlPool) {
    let stripe = Shared::default();
    suspended(&pool, &stripe).await;
    {
        let mut fake = stripe.lock().unwrap();
        fake.prices.insert(
            "price_flat".into(),
            json!({ "id": "price_flat", "active": true, "unit_amount": 5900,
                    "recurring": { "interval": "month", "interval_count": 1 } }),
        );
        // Not a plain monthly price: the configured 9900 stays.
        fake.prices.insert(
            "price_plus".into(),
            json!({ "id": "price_plus", "active": true, "unit_amount": 99000,
                    "recurring": { "interval": "year", "interval_count": 1 } }),
        );
    }
    let server = server(&pool, Some(&stripe)).await;
    let billing: Value = server
        .get("/v1/billing")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(
        billing["plans"],
        json!([
            { "price_id": "price_flat", "name": "Granite Manager", "monthly_price_cents": 5900 },
            { "price_id": "price_plus", "name": "Granite Manager Plus", "monthly_price_cents": 9900 },
        ])
    );
    let quote: Value = server
        .get("/v1/billing/reactivation")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(quote["plan"]["monthly_cents"], 5900);
    // Read once, then reused for 10 minutes.
    assert_eq!(sent(&stripe, "GET", "/v1/prices/price_flat").len(), 1);
}

#[test_db::test(seed)]
async fn the_page_reads_stripe_every_five_minutes_or_now_back_from_checkout(pool: MySqlPool) {
    let stripe = Shared::default();
    subscribed(&pool, &stripe).await;
    let server = server(&pool, Some(&stripe)).await;
    let price = |billing: &Value| billing["subscription"]["price_id"].clone();
    let first: Value = server
        .get("/v1/billing")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(price(&first), "price_flat");
    let reads = sent(&stripe, "GET", "/v1/subscriptions").len();
    assert!(reads > 0);

    // The plan changed at Stripe and no webhook came.
    stripe
        .lock()
        .unwrap()
        .subscriptions
        .get_mut("sub_100")
        .unwrap()["items"]["data"][0]["price"] =
        json!({ "id": "price_plus", "nickname": "Granite Manager Plus", "unit_amount": 9900 });
    let throttled: Value = server
        .get("/v1/billing")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(price(&throttled), "price_flat");
    assert_eq!(sent(&stripe, "GET", "/v1/subscriptions").len(), reads);

    let forced: Value = server
        .get("/v1/billing?checkout=success")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(price(&forced), "price_plus");
    assert_eq!(forced["subscription"]["plan_name"], "Granite Manager Plus");
}

#[test_db::test(seed)]
async fn the_last_payment_is_caught_up_but_never_moved_back(pool: MySqlPool) {
    let stripe = Shared::default();
    subscribed(&pool, &stripe).await;
    sqlx::query(
        "UPDATE company_billing SET last_successful_payment_at = '2026-09-20 00:00:00' WHERE company_id = 100",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(&pool, Some(&stripe)).await;
    let billing: Value = server
        .get("/v1/billing")
        .with_session(sessions::MANAGER)
        .await
        .json();
    // Stripe's paid invoice is from September 1.
    assert_eq!(
        billing["subscription"]["last_successful_payment_at"],
        "2026-09-20T00:00:00Z"
    );
}

#[test_db::test(seed)]
async fn when_stripe_cannot_be_reached_the_page_shows_what_is_stored(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 5, 100);
         INSERT INTO company_billing (company_id, stripe_customer_id, stripe_subscription_id,
                                      stripe_payment_method_id, subscription_status, payment_status,
                                      plan_name, monthly_price_cents)
         VALUES (100, 'cus_100', 'sub_100', 'pm_100', 'active', 'active', 'Granite Manager', 4900);
         INSERT INTO company_billing_invoices (company_id, stripe_invoice_id, status, amount_due_cents,
                                               amount_paid_cents, paid_at)
         VALUES (100, 'in_stored', 'paid', 0, 4900, '2026-09-01 00:00:00');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_billing_plans(plans())
        .with_stripe(Stripe::new(Some("sk_test_fake"), UNREACHABLE));
    let server = TestServer::new(app(state));
    let billing: Value = server
        .get("/v1/billing?checkout=success")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(billing["subscription"]["payment_status"], "active");
    assert_eq!(billing["subscription"]["plan_name"], "Granite Manager");
    assert_eq!(
        billing["invoices"],
        json!([{
            "stripe_invoice_id": "in_stored", "number": null, "status": "paid",
            "amount_due_cents": 0, "amount_paid_cents": 4900, "hosted_invoice_url": null,
            "invoice_pdf": null, "paid_at": "2026-09-01T00:00:00Z", "created_at": null,
        }])
    );
    assert_eq!(billing["plans"][0]["monthly_price_cents"], 4900);
    assert_eq!(billing["card"], Value::Null);
}

#[test_db::test(seed)]
async fn the_page_lists_stripe_invoice_numbers_and_the_customers_default_card(pool: MySqlPool) {
    let stripe = Shared::default();
    subscribed(&pool, &stripe).await;
    sqlx::query(
        "UPDATE company_billing SET stripe_payment_method_id = NULL WHERE company_id = 100",
    )
    .execute(&pool)
    .await
    .unwrap();
    {
        let mut fake = stripe.lock().unwrap();
        // The portal's card form set only the customer's default.
        fake.subscriptions.get_mut("sub_100").unwrap()["default_payment_method"] = Value::Null;
        fake.customers.insert(
            "cus_100".into(),
            json!({ "id": "cus_100", "object": "customer",
                    "invoice_settings": { "default_payment_method": "pm_customer" } }),
        );
        fake.payment_methods.insert(
            "pm_customer".into(),
            json!({ "id": "pm_customer", "object": "payment_method", "type": "card", "customer": "cus_100",
                    "card": { "brand": "mastercard", "last4": "5555", "exp_month": 12, "exp_year": 2031 } }),
        );
        for (id, number, created) in [
            ("in_paid", "GM-0001", 1_788_000_000),
            ("in_open", "GM-0002", 1_790_600_000),
        ] {
            let invoice = fake.invoices.get_mut(id).unwrap();
            invoice["number"] = json!(number);
            invoice["created"] = json!(created);
        }
        let mut draft = invoice("in_draft", false, 4900);
        draft["status"] = json!("draft");
        draft["created"] = json!(1_790_700_000);
        fake.invoices.insert("in_draft".into(), draft);
    }
    let server = server(&pool, Some(&stripe)).await;
    let billing: Value = server
        .get("/v1/billing")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(
        billing["card"],
        json!({ "id": "pm_customer", "brand": "mastercard", "last4": "5555", "exp_month": 12, "exp_year": 2031 })
    );
    assert_eq!(
        sent(&stripe, "GET", "/v1/customers/cus_100")[0]["expand[]"],
        "invoice_settings.default_payment_method"
    );
    assert_eq!(
        billing["invoices"],
        json!([
            {
                "stripe_invoice_id": "in_open", "number": "GM-0002", "status": "open",
                "amount_due_cents": 4900, "amount_paid_cents": 0,
                "hosted_invoice_url": "https://pay.stripe.com/i", "invoice_pdf": null,
                "paid_at": null, "created_at": "2026-09-28T12:53:20Z",
            },
            {
                "stripe_invoice_id": "in_paid", "number": "GM-0001", "status": "paid",
                "amount_due_cents": 0, "amount_paid_cents": 4900,
                "hosted_invoice_url": "https://pay.stripe.com/i", "invoice_pdf": null,
                "paid_at": "2026-09-01T00:00:00Z", "created_at": "2026-08-29T10:40:00Z",
            },
        ])
    );
}

#[test_db::test(seed)]
async fn paying_again_confirms_the_open_first_payment_instead_of_a_new_subscription(
    pool: MySqlPool,
) {
    let stripe = Shared::default();
    suspended(&pool, &stripe).await;
    {
        let mut fake = stripe.lock().unwrap();
        fake.subscriptions.insert(
            "sub_pending".into(),
            json!({ "id": "sub_pending", "object": "subscription", "status": "incomplete",
                    "customer": "cus_100", "metadata": { "company_id": "100" },
                    "latest_invoice": "in_pending",
                    "items": { "data": [{ "id": "si_pending", "quantity": 1,
                        "price": { "id": "price_flat", "nickname": "Granite Manager", "unit_amount": 4900 } }] } }),
        );
        fake.invoices.insert(
            "in_pending".into(),
            json!({ "id": "in_pending", "object": "invoice", "customer": "cus_100", "status": "open",
                    "amount_due": 4900, "amount_paid": 0,
                    "confirmation_secret": { "client_secret": "pi_pending_secret" } }),
        );
    }
    let server = server(&pool, Some(&stripe)).await;
    let started: Value = server
        .post("/v1/billing/reactivation")
        .with_session(sessions::MANAGER)
        .json(&json!({ "payment_method_id": "pm_new" }))
        .await
        .json();
    assert_eq!(
        started,
        json!({ "status": "requires_payment", "client_secret": "pi_pending_secret", "subscription_id": "sub_pending" })
    );
    assert_eq!(
        sent(&stripe, "POST", "/v1/subscriptions/sub_pending")[0]["default_payment_method"],
        "pm_new"
    );
    assert!(
        sent(&stripe, "GET", "/v1/invoices/in_pending")
            .iter()
            .any(|query| query.get("expand[]").map(String::as_str) == Some("confirmation_secret"))
    );
    assert!(sent(&stripe, "POST", "/v1/subscriptions").is_empty());
    assert!(sent(&stripe, "DELETE", "/v1/subscriptions/sub_pending").is_empty());
    let row = billing_row(&pool, 100).await.unwrap();
    assert_eq!(row.stripe_subscription_id.as_deref(), Some("sub_pending"));
    assert_eq!(row.subscription_status, "incomplete");
}

#[test_db::test(seed)]
async fn a_busy_company_lock_keeps_a_paused_company_on_the_pay_page(pool: MySqlPool) {
    let stripe = Shared::default();
    suspended(&pool, &stripe).await;
    let server = server(&pool, Some(&stripe)).await;
    // Another request holds the lock past the sync's 10 second wait.
    let lock = CompanyLock::acquire(&pool, 100).await.unwrap();
    let held = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10_500)).await;
        lock.release().await;
    });
    let quote: Value = server
        .get("/v1/billing/reactivation")
        .with_session(sessions::MANAGER)
        .await
        .json();
    held.await.unwrap();
    assert_eq!(quote["needed"], true);
    assert_eq!(quote["plan"]["price_id"], "price_flat");
}

#[test_db::test(seed)]
async fn reconciliation_rereads_every_stripe_billed_company_past_failures(pool: MySqlPool) {
    let stripe = Shared::default();
    subscribed(&pool, &stripe).await;
    // Company 101's subscription is gone at Stripe: that company fails.
    sqlx::raw_sql(
        "UPDATE company SET billing_required = 1 WHERE id = 101;
         INSERT INTO company_billing (company_id, stripe_customer_id, stripe_subscription_id,
                                      subscription_status, payment_status)
         VALUES (101, 'cus_101', 'sub_missing', 'active', 'active');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let client = Stripe::new(
        Some("sk_test_fake"),
        &fake_stripe(Arc::clone(&stripe)).await,
    );
    assert_eq!(
        reconcile_with_stripe(&client, &pool).await.unwrap(),
        Some(Reconciled {
            checked: 1,
            failed: 1
        })
    );
    let paid_at: Option<chrono::NaiveDateTime> = sqlx::query_scalar(
        "SELECT last_successful_payment_at FROM company_billing WHERE company_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(paid_at.unwrap().to_string(), "2026-09-01 00:00:00");

    // A company Stripe could not be read for counts as failed, not checked.
    let down = Stripe::new(Some("sk_test_fake"), UNREACHABLE);
    assert_eq!(
        reconcile_with_stripe(&down, &pool).await.unwrap(),
        Some(Reconciled {
            checked: 0,
            failed: 2
        })
    );
    assert_eq!(
        reconcile_with_stripe(&Stripe::disabled(), &pool)
            .await
            .unwrap(),
        None
    );
}

#[test_db::test(seed)]
async fn reconciliation_batches_resume_after_the_last_company(pool: MySqlPool) {
    let stripe = Shared::default();
    subscribed(&pool, &stripe).await;
    sqlx::raw_sql(
        "UPDATE company SET billing_required = 1 WHERE id = 101;
         INSERT INTO company_billing (company_id, stripe_customer_id, stripe_subscription_id,
                                      subscription_status, payment_status)
         VALUES (101, 'cus_101', 'sub_missing', 'active', 'active');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let client = Stripe::new(
        Some("sk_test_fake"),
        &fake_stripe(Arc::clone(&stripe)).await,
    );
    let batch = |after| reconcile_batch(&client, &pool, after, 1, None);

    let first = batch(0).await.unwrap().unwrap();
    assert_eq!((first.last_id, first.done), (Some(100), false));
    let second = batch(100).await.unwrap().unwrap();
    assert_eq!((second.last_id, second.done), (Some(101), false));
    assert_eq!(second.reconciled.failed, 1);
    let last = batch(101).await.unwrap().unwrap();
    assert_eq!((last.last_id, last.done), (None, true));
    // A spent budget stops before the first company: no progress, not done.
    let spent = reconcile_batch(&client, &pool, 0, 10, Some(Duration::ZERO))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((spent.last_id, spent.done), (None, false));
}
