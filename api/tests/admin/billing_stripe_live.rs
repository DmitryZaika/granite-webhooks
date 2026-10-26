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
