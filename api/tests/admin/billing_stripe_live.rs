//! Restoring a canceled company against Stripe's test mode (port of the
//! CRM's `billingReactivation.stripe.test.ts`). Runs only when
//! `STRIPE_TEST_SECRET_KEY` is a `sk_test_` key; it creates (and cleans up)
//! test customers, a product and a price.

use api::billing::reactivation::{Started, finish, start};
use api::billing::store::{BillingStore, MySqlBillingStore};
use api::billing::stripe::{API_VERSION, Stripe};
use api::billing::{BillingPlan, CompanyBilling, PaymentStatus};
use serde_json::Value;
use sqlx::MySqlPool;
use std::time::Duration;

const BASE: &str = "https://api.stripe.com";

fn test_key() -> Option<String> {
    std::env::var("STRIPE_TEST_SECRET_KEY")
        .ok()
        .filter(|key| key.starts_with("sk_test_"))
}

/// Raw Stripe calls the test needs beyond the client billing uses.
struct Raw {
    key: String,
    http: reqwest::Client,
}

impl Raw {
    async fn call(&self, method: reqwest::Method, path: &str, form: &[(&str, &str)]) -> Value {
        let body = serde_html_form::to_string(form).unwrap();
        let get = method == reqwest::Method::GET;
        let url = if get {
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

    async fn post(&self, path: &str, form: &[(&str, &str)]) -> Value {
        self.call(reqwest::Method::POST, path, form).await
    }

    /// Confirms the first invoice's payment as Stripe.js would in the browser.
    async fn confirm(&self, client_secret: &str, payment_method: &str) -> Value {
        let intent = client_secret.split("_secret_").next().unwrap();
        self.post(
            &format!("/v1/payment_intents/{intent}/confirm"),
            &[("payment_method", payment_method)],
        )
        .await
    }

    async fn active_subscriptions(&self, customer: &str) -> usize {
        let list = self
            .call(
                reqwest::Method::GET,
                "/v1/subscriptions",
                &[("customer", customer), ("status", "active")],
            )
            .await;
        list["data"].as_array().map_or(0, Vec::len)
    }
}

async fn canceled_company(raw: &Raw, pool: &MySqlPool, company_id: i32, price_id: &str) -> String {
    let customer = raw
        .post(
            "/v1/customers",
            &[
                ("name", &format!("Reactivation test {company_id}")),
                ("metadata[test]", "billing-reactivation"),
            ],
        )
        .await;
    let customer_id = customer["id"].as_str().unwrap().to_string();
    sqlx::query("INSERT IGNORE INTO company (id, name) VALUES (?, 'Reactivation test')")
        .bind(company_id)
        .execute(pool)
        .await
        .unwrap();
    MySqlBillingStore(pool)
        .save(&CompanyBilling {
            stripe_customer_id: Some(customer_id.clone()),
            stripe_subscription_id: Some(format!("sub_canceled_{company_id}")),
            stripe_price_id: Some(price_id.into()),
            subscription_status: "canceled".into(),
            payment_status: PaymentStatus::Suspended,
            user_count: Some(3),
            ..CompanyBilling::empty(company_id)
        })
        .await
        .unwrap();
    customer_id
}

async fn payment_status(pool: &MySqlPool, company_id: i32) -> PaymentStatus {
    MySqlBillingStore(pool)
        .by_company(company_id)
        .await
        .unwrap()
        .unwrap()
        .payment_status
}

#[test_db::test]
async fn canceled_companies_pay_to_restore_access_in_stripe_test_mode(pool: MySqlPool) {
    let Some(key) = test_key() else {
        eprintln!("STRIPE_TEST_SECRET_KEY is not a test key; skipped");
        return;
    };
    let raw = Raw {
        key: key.clone(),
        http: common::http::client(),
    };
    let stripe = Stripe::new(Some(&key), BASE);
    let store = MySqlBillingStore(&pool);
    let product = raw
        .post("/v1/products", &[("name", "Granite Manager test")])
        .await;
    let product_id = product["id"].as_str().unwrap().to_string();
    let price = raw
        .post(
            "/v1/prices",
            &[
                ("product", &product_id),
                ("currency", "usd"),
                ("unit_amount", "4900"),
                ("recurring[interval]", "month"),
                ("nickname", "Granite Manager"),
            ],
        )
        .await;
    let price_id = price["id"].as_str().unwrap().to_string();
    let plans = vec![BillingPlan {
        price_id: price_id.clone(),
        name: "Granite Manager".into(),
        monthly_price_cents: Some(4900),
    }];
    let mut customers = Vec::new();

    let result = async {
        // A saved card: Pay charges once and unlocks the company.
        let customer = canceled_company(&raw, &pool, 901, &price_id).await;
        customers.push(customer.clone());
        let saved = raw
            .post(
                "/v1/payment_methods/pm_card_visa/attach",
                &[("customer", &customer)],
            )
            .await;
        let saved_id = saved["id"].as_str().unwrap().to_string();
        let Started::RequiresPayment {
            client_secret,
            subscription_id,
        } = start(&stripe, &store, &plans, 901, &saved_id)
            .await
            .unwrap()
        else {
            panic!("expected a payment to confirm");
        };
        let confirmed = raw.confirm(&client_secret, &saved_id).await;
        assert_eq!(confirmed["status"], "succeeded");
        assert_eq!(confirmed["amount"], 4900);
        let finished = finish(&stripe, &store, 901, Some(&subscription_id))
            .await
            .unwrap();
        assert_eq!(finished, PaymentStatus::Active);
        let record = store.by_company(901).await.unwrap().unwrap();
        assert_eq!(record.payment_status, PaymentStatus::Active);
        assert_eq!(record.subscription_status, "active");
        assert_eq!(
            record.stripe_subscription_id.as_deref(),
            Some(subscription_id.as_str())
        );
        assert_eq!(record.user_count, Some(1));
        assert_eq!(record.monthly_price_cents, Some(4900));
        let again = start(&stripe, &store, &plans, 901, &saved_id)
            .await
            .unwrap();
        assert_eq!(again, Started::Paid);
        assert_eq!(raw.active_subscriptions(&customer).await, 1);

        // A declined card keeps the company locked; a new card then works
        // with one subscription.
        let customer = canceled_company(&raw, &pool, 902, &price_id).await;
        customers.push(customer.clone());
        let declined = raw
            .post(
                "/v1/payment_methods",
                &[("type", "card"), ("card[token]", "tok_chargeDeclined")],
            )
            .await;
        let err = start(
            &stripe,
            &store,
            &plans,
            902,
            declined["id"].as_str().unwrap(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Your card was declined"), "{err}");
        assert_eq!(payment_status(&pool, 902).await, PaymentStatus::Suspended);
        let fails = raw
            .post(
                "/v1/payment_methods",
                &[("type", "card"), ("card[token]", "tok_chargeCustomerFail")],
            )
            .await;
        let fails_id = fails["id"].as_str().unwrap();
        let Started::RequiresPayment {
            client_secret,
            subscription_id: first,
        } = start(&stripe, &store, &plans, 902, fails_id).await.unwrap()
        else {
            panic!("expected a payment to confirm");
        };
        let refused = raw.confirm(&client_secret, fails_id).await;
        assert!(refused.get("error").is_some(), "{refused}");
        assert_eq!(payment_status(&pool, 902).await, PaymentStatus::Suspended);
        let new_card = raw
            .post(
                "/v1/payment_methods",
                &[("type", "card"), ("card[token]", "tok_visa")],
            )
            .await;
        let new_card_id = new_card["id"].as_str().unwrap();
        let Started::RequiresPayment {
            client_secret,
            subscription_id: second,
        } = start(&stripe, &store, &plans, 902, new_card_id)
            .await
            .unwrap()
        else {
            panic!("expected a payment to confirm");
        };
        assert_ne!(second, first);
        raw.confirm(&client_secret, new_card_id).await;
        assert_eq!(
            finish(&stripe, &store, 902, Some(&second)).await.unwrap(),
            PaymentStatus::Active
        );
        assert_eq!(payment_status(&pool, 902).await, PaymentStatus::Active);
        let abandoned = stripe.subscription(&first).await.unwrap();
        assert_ne!(abandoned["status"], "active");
        assert_eq!(raw.active_subscriptions(&customer).await, 1);

        // Another customer's card is refused.
        let customer = canceled_company(&raw, &pool, 903, &price_id).await;
        customers.push(customer);
        let other = raw
            .post("/v1/customers", &[("name", "Other company")])
            .await;
        let other_id = other["id"].as_str().unwrap().to_string();
        customers.push(other_id.clone());
        let foreign = raw
            .post(
                "/v1/payment_methods/pm_card_visa/attach",
                &[("customer", &other_id)],
            )
            .await;
        let err = start(
            &stripe,
            &store,
            &plans,
            903,
            foreign["id"].as_str().unwrap(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "This card cannot be used");
        assert_eq!(payment_status(&pool, 903).await, PaymentStatus::Suspended);
    };
    // Clean up (a failed assertion leaves only test-mode objects behind).
    let outcome = tokio::time::timeout(Duration::from_secs(240), result).await;
    for customer in &customers {
        raw.call(
            reqwest::Method::DELETE,
            &format!("/v1/customers/{customer}"),
            &[],
        )
        .await;
    }
    raw.post(&format!("/v1/prices/{price_id}"), &[("active", "false")])
        .await;
    raw.post(
        &format!("/v1/products/{product_id}"),
        &[("active", "false")],
    )
    .await;
    outcome.expect("timed out");
}

// ---- Card and bank account payment methods (step 13), Stripe's test mode.

/// How long a bank test may wait for Stripe, and how often it asks.
const BANK_WAIT: Duration = Duration::from_secs(90);
const POLL_STEP: Duration = Duration::from_secs(2);

/// Asks `read` until `done` holds (or `BANK_WAIT` passes); the last answer.
async fn poll<F, Fut>(read: F, done: impl Fn(&Value) -> bool) -> Value
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Value>,
{
    let started = std::time::Instant::now();
    loop {
        let value = read().await;
        if done(&value) || started.elapsed() >= BANK_WAIT {
            return value;
        }
        tokio::time::sleep(POLL_STEP).await;
    }
}

impl Raw {
    async fn customer(&self, name: &str, clock: Option<&str>) -> String {
        let mut form = vec![
            ("name", name),
            ("email", "payments+test_email@example.test"),
            ("metadata[test]", "payment-methods"),
        ];
        if let Some(clock) = clock {
            form.push(("test_clock", clock));
        }
        let customer = self.post("/v1/customers", &form).await;
        customer["id"].as_str().unwrap().to_string()
    }

    /// A bank account typed in by hand, confirmed on the SetupIntent with an
    /// offline mandate; `microdeposits` forces micro-deposit verification.
    async fn confirm_bank(&self, setup_intent: &str, account: &str) -> Value {
        self.post(
            &format!("/v1/setup_intents/{setup_intent}/confirm"),
            &[
                ("payment_method_data[type]", "us_bank_account"),
                ("payment_method_data[us_bank_account][routing_number]", "110000000"),
                ("payment_method_data[us_bank_account][account_number]", account),
                ("payment_method_data[us_bank_account][account_holder_type]", "company"),
                ("payment_method_data[billing_details][name]", "Payment Methods Test"),
                ("mandate_data[customer_acceptance][type]", "offline"),
            ],
        )
        .await
    }

    async fn verify_descriptor(&self, setup_intent: &str, code: &str) -> Value {
        self.post(
            &format!("/v1/setup_intents/{setup_intent}/verify_microdeposits"),
            &[("descriptor_code", code)],
        )
        .await
    }

    /// An off-session bank debit of $10 with the verified method.
    async fn debit(&self, customer: &str, method: &str) -> Value {
        self.post(
            "/v1/payment_intents",
            &[
                ("amount", "1000"),
                ("currency", "usd"),
                ("customer", customer),
                ("payment_method", method),
                ("payment_method_types[]", "us_bank_account"),
                ("confirm", "true"),
                ("off_session", "true"),
            ],
        )
        .await
    }
}

#[tokio::test]
async fn card_and_bank_setups_behave_in_stripe_test_mode() {
    let Some(key) = test_key() else {
        eprintln!("STRIPE_TEST_SECRET_KEY is not a test key; skipped");
        return;
    };
    let raw = Raw {
        key: key.clone(),
        http: common::http::client(),
    };
    let stripe = Stripe::new(Some(&key), BASE);
    let mut customers = Vec::new();
    let mut clocks = Vec::new();
    let mut cleanup = Vec::new();

    let result = async {
        let customer = raw.customer("Payment methods test", None).await;
        customers.push(customer.clone());
        let setup = |purpose: &'static str| {
            let stripe = stripe.clone();
            let customer = customer.clone();
            async move {
                stripe
                    .create_company_setup_intent(
                        &customer,
                        20_790,
                        purpose,
                        None,
                        &format!("live-test:{}", uuid::Uuid::new_v4()),
                    )
                    .await
                    .unwrap()
            }
        };

        // A card: the SetupIntent succeeds, nothing is charged.
        let card = setup("payment_method_update").await;
        let card_id = card["id"].as_str().unwrap();
        assert_eq!(card["metadata"]["company_id"], "20790");
        assert!(card.get("statement_descriptor").is_none());
        raw.post(
            &format!("/v1/setup_intents/{card_id}/confirm"),
            &[("payment_method", "pm_card_visa")],
        )
        .await;
        let card = stripe.setup_intent(card_id).await.unwrap();
        assert_eq!(card["status"], "succeeded");
        assert_eq!(card["payment_method"]["card"]["last4"], "4242");

        // 3D Secure: the SetupIntent waits for the cardholder.
        let three_ds = setup("payment_method_update").await;
        let three_ds_id = three_ds["id"].as_str().unwrap();
        let confirmed = raw
            .post(
                &format!("/v1/setup_intents/{three_ds_id}/confirm"),
                &[("payment_method", "pm_card_authenticationRequired")],
            )
            .await;
        assert_eq!(confirmed["status"], "requires_action", "{confirmed}");

        // A bank account that connects at once (test token).
        let instant = setup("payment_method_update").await;
        let instant_id = instant["id"].as_str().unwrap();
        raw.post(
            &format!("/v1/setup_intents/{instant_id}/confirm"),
            &[
                ("payment_method", "pm_usBankAccount_success"),
                ("mandate_data[customer_acceptance][type]", "offline"),
            ],
        )
        .await;
        let instant = stripe.setup_intent(instant_id).await.unwrap();
        assert_eq!(instant["status"], "succeeded", "{instant}");
        assert_eq!(instant["payment_method"]["type"], "us_bank_account");

        // Micro-deposits: verifying, then SM11AA verifies it.
        let pending = setup("pay").await;
        let pending_id = pending["id"].as_str().unwrap();
        raw.confirm_bank(pending_id, "000123456789").await;
        let pending = stripe.setup_intent(pending_id).await.unwrap();
        assert!(api::billing::methods::is_microdeposits(&pending), "{pending}");
        assert!(api::billing::methods::verify_url(&pending).is_some());
        let verified = raw.verify_descriptor(pending_id, "SM11AA").await;
        assert_eq!(verified["status"], "succeeded", "{verified}");

        // Failing accounts, each verified first; clocks do not simulate ACH,
        // so the debit is polled.
        for (account, expect) in [
            ("000222222227", "insufficient_funds"),
            ("000111111113", "account_closed"),
            ("000555555559", "dispute"),
        ] {
            let bank_customer = raw.customer("Bank failure test", None).await;
            customers.push(bank_customer.clone());
            let si = stripe
                .create_company_setup_intent(
                    &bank_customer,
                    20_791,
                    "pay",
                    None,
                    &format!("live-test:{}", uuid::Uuid::new_v4()),
                )
                .await
                .unwrap();
            let si_id = si["id"].as_str().unwrap();
            raw.confirm_bank(si_id, account).await;
            let verified = raw.verify_descriptor(si_id, "SM11AA").await;
            assert_eq!(verified["status"], "succeeded", "{account}: {verified}");
            let method = stripe_id_of(&verified["payment_method"]);
            let intent = raw.debit(&bank_customer, &method).await;
            let intent_id = intent["id"].as_str().unwrap().to_string();
            let settled = poll(
                || {
                    let stripe = stripe.clone();
                    let intent_id = intent_id.clone();
                    async move { stripe.payment_intent(&intent_id).await.unwrap() }
                },
                |intent| intent["status"] != "processing",
            )
            .await;
            if expect == "dispute" {
                assert_eq!(settled["status"], "succeeded", "{account}: {settled}");
                let charge_id = stripe_id_of(&settled["latest_charge"]);
                let charge = poll(
                    || {
                        let stripe = stripe.clone();
                        let charge_id = charge_id.clone();
                        async move { stripe.charge(&charge_id).await.unwrap() }
                    },
                    |charge| charge["disputed"] == true,
                )
                .await;
                assert_eq!(charge["disputed"], true, "{account}: {charge}");
            } else {
                assert_eq!(settled["status"], "requires_payment_method", "{account}: {settled}");
                let charge_id = stripe_id_of(&settled["latest_charge"]);
                let charge = stripe.charge(&charge_id).await.unwrap();
                assert_eq!(charge["payment_method_details"]["type"], "us_bank_account");
                assert_eq!(charge["failure_code"], expect, "{account}: {charge}");
            }
        }

        // A card that attaches but fails its first charge: past due after
        // the clock passes the trial end.
        let now = chrono::Utc::now().timestamp();
        let clock = raw
            .post(
                "/v1/test_helpers/test_clocks",
                &[("frozen_time", &now.to_string()), ("name", "payment methods")],
            )
            .await;
        let clock_id = clock["id"].as_str().unwrap().to_string();
        clocks.push(clock_id.clone());
        let failing = raw.customer("Charge fail test", Some(&clock_id)).await;
        let si = stripe
            .create_company_setup_intent(
                &failing,
                20_792,
                "payment_method_update",
                None,
                &format!("live-test:{}", uuid::Uuid::new_v4()),
            )
            .await
            .unwrap();
        let si_id = si["id"].as_str().unwrap();
        raw.post(
            &format!("/v1/setup_intents/{si_id}/confirm"),
            &[("payment_method", "pm_card_chargeCustomerFail")],
        )
        .await;
        let si = stripe.setup_intent(si_id).await.unwrap();
        assert_eq!(si["status"], "succeeded", "{si}");
        let method = stripe_id_of(&si["payment_method"]);
        let product = raw.post("/v1/products", &[("name", "Payment methods test")]).await;
        let product_id = product["id"].as_str().unwrap().to_string();
        cleanup.push(product_id.clone());
        let trial_end = (now + 86_400).to_string();
        let subscription = raw
            .post(
                "/v1/subscriptions",
                &[
                    ("customer", &failing),
                    ("default_payment_method", &method),
                    ("trial_end", &trial_end),
                    ("items[0][price_data][currency]", "usd"),
                    ("items[0][price_data][product]", &product_id),
                    ("items[0][price_data][unit_amount]", "30000"),
                    ("items[0][price_data][recurring][interval]", "month"),
                ],
            )
            .await;
        let subscription_id = subscription["id"].as_str().unwrap().to_string();
        assert_eq!(subscription["status"], "trialing", "{subscription}");
        let frozen = (now + 2 * 86_400).to_string();
        raw.post(
            &format!("/v1/test_helpers/test_clocks/{clock_id}/advance"),
            &[("frozen_time", &frozen)],
        )
        .await;
        let past_due = poll(
            || {
                let stripe = stripe.clone();
                let id = subscription_id.clone();
                async move { stripe.subscription(&id).await.unwrap() }
            },
            |subscription| subscription["status"] == "past_due",
        )
        .await;
        assert_eq!(past_due["status"], "past_due", "{past_due}");
    };
    let outcome = tokio::time::timeout(Duration::from_secs(600), result).await;
    for customer in &customers {
        raw.call(reqwest::Method::DELETE, &format!("/v1/customers/{customer}"), &[])
            .await;
    }
    for clock in &clocks {
        raw.call(
            reqwest::Method::DELETE,
            &format!("/v1/test_helpers/test_clocks/{clock}"),
            &[],
        )
        .await;
    }
    for product in &cleanup {
        raw.post(&format!("/v1/products/{product}"), &[("active", "false")])
            .await;
    }
    outcome.expect("timed out");
}

/// An id field, or an expanded object's id.
fn stripe_id_of(value: &Value) -> String {
    api::billing::stripe::stripe_id(value).expect("an id")
}
