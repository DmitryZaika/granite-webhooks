//! The Stripe REST API (the calls billing makes), webhook signature checks,
//! and Stripe objects read into billing snapshots.
//!
//! Requests pin the API version the CRM's Stripe SDK used, so objects have
//! the same shape (`invoice.parent.subscription_details`, periods on
//! subscription items, `invoice.confirmation_secret`).

use super::{BillingEvent, InvoiceSnapshot, SubscriptionSnapshot};
use chrono::{DateTime, NaiveDateTime};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

const API_BASE: &str = "https://api.stripe.com";
/// `stripe` npm 22's `ApiVersion`.
pub const API_VERSION: &str = "2026-08-26.dahlia";
/// How old a webhook signature may be (the SDK's default).
const SIGNATURE_TOLERANCE_SECS: i64 = 300;

#[derive(Debug, thiserror::Error)]
pub enum StripeError {
    #[error("Billing is not configured")]
    NotConfigured,
    /// Stripe refused the request; the message is Stripe's, for the user
    /// (e.g. "Your card was declined.").
    #[error("{0}")]
    Api(String),
    #[error("Stripe request failed: {0}")]
    Request(String),
}

/// A Stripe client; disabled without `STRIPE_SECRET_KEY`.
#[derive(Clone)]
pub struct Stripe {
    key: Option<Arc<str>>,
    base: Arc<str>,
    http: reqwest::Client,
    /// Pause between polls of a subscription that is not paid yet.
    pub poll_delay: Duration,
}

impl Stripe {
    pub fn disabled() -> Self {
        Self::new(None, API_BASE)
    }

    pub fn from_env() -> Self {
        Self::new(std::env::var("STRIPE_SECRET_KEY").ok().as_deref(), API_BASE)
    }

    /// `base` is `https://api.stripe.com` (a stub in tests).
    pub fn new(key: Option<&str>, base: &str) -> Self {
        Self {
            key: key
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(Arc::from),
            base: Arc::from(base.trim_end_matches('/')),
            http: common::http::client(),
            poll_delay: Duration::from_millis(1500),
        }
    }

    pub const fn is_enabled(&self) -> bool {
        self.key.is_some()
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        form: &[(&str, String)],
    ) -> Result<Value, StripeError> {
        let key = self.key.as_ref().ok_or(StripeError::NotConfigured)?;
        let encoded = serde_html_form::to_string(form)
            .map_err(|err| StripeError::Request(err.to_string()))?;
        let get = method == reqwest::Method::GET;
        let url = if get && !encoded.is_empty() {
            format!("{}{path}?{encoded}", self.base)
        } else {
            format!("{}{path}", self.base)
        };
        let request = self
            .http
            .request(method, &url)
            .bearer_auth(&**key)
            .header("Stripe-Version", API_VERSION)
            .timeout(Duration::from_secs(30));
        let request = if get {
            request
        } else {
            request
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(encoded)
        };
        let response = request
            .send()
            .await
            .map_err(|err| StripeError::Request(err.without_url().to_string()))?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|err| StripeError::Request(err.without_url().to_string()))?;
        if status.is_success() {
            Ok(body)
        } else {
            let message = body["error"]["message"]
                .as_str()
                .unwrap_or("Stripe request failed")
                .to_string();
            Err(StripeError::Api(message))
        }
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, StripeError> {
        self.send(reqwest::Method::GET, path, query).await
    }

    async fn post(&self, path: &str, form: &[(&str, String)]) -> Result<Value, StripeError> {
        self.send(reqwest::Method::POST, path, form).await
    }

    /// The customer's subscriptions, any status, newest first (up to 20).
    pub async fn list_subscriptions(&self, customer: &str) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get(
                "/v1/subscriptions",
                &[
                    ("customer", customer.into()),
                    ("status", "all".into()),
                    ("limit", "20".into()),
                ],
            )
            .await?;
        Ok(data(list))
    }

    pub async fn subscription(&self, id: &str) -> Result<Value, StripeError> {
        self.get(&format!("/v1/subscriptions/{}", path_part(id)), &[])
            .await
    }

    pub async fn cancel_subscription(&self, id: &str) -> Result<Value, StripeError> {
        self.send(
            reqwest::Method::DELETE,
            &format!("/v1/subscriptions/{}", path_part(id)),
            &[],
        )
        .await
    }

    /// A subscription that waits for the first payment, with the invoice's
    /// confirmation secret for the browser.
    pub async fn create_subscription(
        &self,
        customer: &str,
        price_id: &str,
        quantity: i64,
        payment_method: &str,
        company_id: i32,
    ) -> Result<Value, StripeError> {
        self.post(
            "/v1/subscriptions",
            &[
                ("customer", customer.into()),
                ("items[0][price]", price_id.into()),
                ("items[0][quantity]", quantity.to_string()),
                ("default_payment_method", payment_method.into()),
                ("payment_behavior", "default_incomplete".into()),
                (
                    "payment_settings[save_default_payment_method]",
                    "on_subscription".into(),
                ),
                ("metadata[company_id]", company_id.to_string()),
                ("expand[]", "latest_invoice.confirmation_secret".into()),
            ],
        )
        .await
    }

    pub async fn invoice(&self, id: &str) -> Result<Value, StripeError> {
        self.get(&format!("/v1/invoices/{}", path_part(id)), &[])
            .await
    }

    /// Open invoices of the customer (up to 100).
    pub async fn open_invoices(&self, customer: &str) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get(
                "/v1/invoices",
                &[
                    ("customer", customer.into()),
                    ("status", "open".into()),
                    ("limit", "100".into()),
                ],
            )
            .await?;
        Ok(data(list))
    }

    pub async fn payment_method(&self, id: &str) -> Result<Value, StripeError> {
        self.get(&format!("/v1/payment_methods/{}", path_part(id)), &[])
            .await
    }

    pub async fn attach_payment_method(
        &self,
        id: &str,
        customer: &str,
    ) -> Result<Value, StripeError> {
        self.post(
            &format!("/v1/payment_methods/{}/attach", path_part(id)),
            &[("customer", customer.into())],
        )
        .await
    }

    pub async fn set_default_payment_method(
        &self,
        customer: &str,
        payment_method: &str,
    ) -> Result<Value, StripeError> {
        self.post(
            &format!("/v1/customers/{}", path_part(customer)),
            &[(
                "invoice_settings[default_payment_method]",
                payment_method.into(),
            )],
        )
        .await
    }
}

impl Stripe {
    /// The customer's latest invoices (up to 24), any status.
    pub async fn list_invoices(&self, customer: &str) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get(
                "/v1/invoices",
                &[("customer", customer.into()), ("limit", "24".into())],
            )
            .await?;
        Ok(data(list))
    }

    /// A customer for a company that never subscribed.
    pub async fn create_customer(&self, name: &str, company_id: i32) -> Result<Value, StripeError> {
        self.post(
            "/v1/customers",
            &[
                ("name", name.into()),
                ("metadata[company_id]", company_id.to_string()),
            ],
        )
        .await
    }

    /// A Checkout page that starts a subscription to `price_id`.
    pub async fn create_checkout_session(
        &self,
        customer: &str,
        company_id: i32,
        price_id: &str,
        quantity: i64,
        return_base: &str,
    ) -> Result<Value, StripeError> {
        self.post(
            "/v1/checkout/sessions",
            &[
                ("mode", "subscription".into()),
                ("customer", customer.into()),
                ("client_reference_id", company_id.to_string()),
                ("line_items[0][price]", price_id.into()),
                ("line_items[0][quantity]", quantity.to_string()),
                ("success_url", format!("{return_base}?checkout=success")),
                ("cancel_url", format!("{return_base}?checkout=cancel")),
                ("metadata[company_id]", company_id.to_string()),
                (
                    "subscription_data[metadata][company_id]",
                    company_id.to_string(),
                ),
            ],
        )
        .await
    }

    /// A billing portal page; `payment_method_update` opens the card form.
    pub async fn create_portal_session(
        &self,
        customer: &str,
        return_url: &str,
        payment_method_update: bool,
    ) -> Result<Value, StripeError> {
        let mut form = vec![
            ("customer", customer.to_string()),
            ("return_url", return_url.to_string()),
        ];
        if payment_method_update {
            form.push(("flow_data[type]", "payment_method_update".into()));
        }
        self.post("/v1/billing_portal/sessions", &form).await
    }

    /// Moves the subscription's item to `price_id` (prorated).
    pub async fn update_subscription(
        &self,
        id: &str,
        item_id: &str,
        price_id: &str,
        quantity: i64,
        company_id: i32,
    ) -> Result<Value, StripeError> {
        self.post(
            &format!("/v1/subscriptions/{}", path_part(id)),
            &[
                ("items[0][id]", item_id.into()),
                ("items[0][price]", price_id.into()),
                ("items[0][quantity]", quantity.to_string()),
                ("proration_behavior", "create_prorations".into()),
                ("metadata[company_id]", company_id.to_string()),
            ],
        )
        .await
    }

    /// The customer's saved cards (up to 10).
    pub async fn list_cards(&self, customer: &str) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get(
                &format!("/v1/customers/{}/payment_methods", path_part(customer)),
                &[("type", "card".into()), ("limit", "10".into())],
            )
            .await?;
        Ok(data(list))
    }
}

fn data(list: Value) -> Vec<Value> {
    match list {
        Value::Object(mut map) => match map.remove("data") {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Ids go in URL paths; Stripe ids are `[A-Za-z0-9_]`, anything else is escaped.
fn path_part(id: &str) -> String {
    percent_encoding::utf8_percent_encode(id, percent_encoding::NON_ALPHANUMERIC).to_string()
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignatureError {
    #[error("No signatures found matching the expected signature for payload")]
    Mismatch,
    #[error("Unable to extract timestamp and signatures from header")]
    Malformed,
    #[error("Timestamp outside the tolerance zone")]
    TooOld,
}

/// Checks a `Stripe-Signature` header (`t=<unix>,v1=<hex hmac>,...`) against
/// the endpoint secret, like the SDK's `constructEvent`.
pub fn verify_signature(
    payload: &[u8],
    header: &str,
    secret: &str,
    now_unix: i64,
) -> Result<(), SignatureError> {
    let mut timestamp: Option<i64> = None;
    let mut signatures: Vec<&str> = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", value)) => timestamp = value.parse().ok(),
            Some(("v1", value)) => signatures.push(value),
            _ => {}
        }
    }
    let Some(timestamp) = timestamp else {
        return Err(SignatureError::Malformed);
    };
    if signatures.is_empty() {
        return Err(SignatureError::Malformed);
    }
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| SignatureError::Mismatch)?;
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    let expected = hex(&mac.finalize().into_bytes());
    let matches = signatures
        .iter()
        .any(|sent| constant_time_eq(sent.as_bytes(), expected.as_bytes()));
    if !matches {
        return Err(SignatureError::Mismatch);
    }
    if now_unix - timestamp > SIGNATURE_TOLERANCE_SECS {
        return Err(SignatureError::TooOld);
    }
    Ok(())
}

/// The header Stripe would send for `payload` at `timestamp` (tests, tools).
pub fn signature_header(payload: &[u8], secret: &str, timestamp: i64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    format!("t={timestamp},v1={}", hex(&mac.finalize().into_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---- Stripe objects -> snapshots

/// An id field: a string, or an expanded object's `id` (none when deleted).
pub fn stripe_id(value: &Value) -> Option<String> {
    match value {
        Value::String(id) => Some(id.clone()),
        Value::Object(map) => {
            if map.get("deleted") == Some(&Value::Bool(true)) {
                return None;
            }
            map.get("id").and_then(Value::as_str).map(str::to_string)
        }
        _ => None,
    }
}

/// A positive whole number from a metadata string (`"12"`).
pub fn positive_int(value: &Value) -> Option<i32> {
    value
        .as_str()?
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|id| *id > 0)
}

fn unix_date(value: &Value) -> Option<NaiveDateTime> {
    DateTime::from_timestamp(value.as_i64()?, 0).map(|date| date.naive_utc())
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

fn non_empty(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

pub fn subscription_snapshot(subscription: &Value) -> SubscriptionSnapshot {
    let item = &subscription["items"]["data"][0];
    let price = &item["price"];
    SubscriptionSnapshot {
        id: subscription["id"].as_str().unwrap_or_default().to_string(),
        customer_id: stripe_id(&subscription["customer"]).unwrap_or_default(),
        price_id: text(&price["id"]),
        item_id: text(&item["id"]),
        status: subscription["status"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        quantity: item["quantity"].as_i64(),
        period_start: unix_date(&item["current_period_start"]),
        period_end: unix_date(&item["current_period_end"]),
        payment_method_id: stripe_id(&subscription["default_payment_method"]),
        plan_name: non_empty(&price["nickname"]).or_else(|| non_empty(&item["plan"]["nickname"])),
        unit_amount_cents: price["unit_amount"].as_i64(),
        company_id: positive_int(&subscription["metadata"]["company_id"]),
        deleted: false,
    }
}

pub fn invoice_snapshot(invoice: &Value) -> InvoiceSnapshot {
    let details = &invoice["parent"]["subscription_details"];
    InvoiceSnapshot {
        id: invoice["id"].as_str().unwrap_or_default().to_string(),
        customer_id: stripe_id(&invoice["customer"]).unwrap_or_default(),
        subscription_id: stripe_id(&details["subscription"]),
        company_id: positive_int(&details["metadata"]["company_id"]),
        status: invoice["status"].as_str().unwrap_or("open").to_string(),
        amount_due_cents: invoice["amount_due"].as_i64().unwrap_or_default(),
        amount_paid_cents: invoice["amount_paid"].as_i64().unwrap_or_default(),
        currency: invoice["currency"].as_str().unwrap_or("usd").to_string(),
        hosted_invoice_url: text(&invoice["hosted_invoice_url"]),
        invoice_pdf: text(&invoice["invoice_pdf"]),
        paid_at: unix_date(&invoice["status_transitions"]["paid_at"]),
    }
}

/// The billing event in a webhook payload; `None` for events billing ignores.
pub fn normalize_event(event: &Value) -> Option<BillingEvent> {
    let id = event["id"].as_str()?.to_string();
    let object = &event["data"]["object"];
    let kind = object["object"].as_str().unwrap_or_default();
    match (event["type"].as_str()?, kind) {
        ("checkout.session.completed", "checkout.session") => {
            let company_id = positive_int(&object["metadata"]["company_id"])
                .or_else(|| positive_int(&object["client_reference_id"]))?;
            let customer_id = stripe_id(&object["customer"])?;
            Some(BillingEvent::CheckoutCompleted {
                id,
                company_id,
                customer_id,
                subscription_id: stripe_id(&object["subscription"]),
            })
        }
        (
            kind @ ("customer.subscription.created"
            | "customer.subscription.updated"
            | "customer.subscription.deleted"),
            "subscription",
        ) => Some(BillingEvent::Subscription {
            id,
            kind: match kind {
                "customer.subscription.created" => "customer.subscription.created",
                "customer.subscription.updated" => "customer.subscription.updated",
                _ => "customer.subscription.deleted",
            },
            subscription: subscription_snapshot(object),
        }),
        (kind @ ("invoice.paid" | "invoice.payment_failed"), "invoice") => {
            Some(BillingEvent::Invoice {
                id,
                paid: kind == "invoice.paid",
                invoice: invoice_snapshot(object),
            })
        }
        ("payment_method.updated", "payment_method") => Some(BillingEvent::PaymentMethodUpdated {
            id,
            customer_id: stripe_id(&object["customer"])?,
            payment_method_id: object["id"].as_str()?.to_string(),
        }),
        _ => None,
    }
}

/// The event with Stripe's current state.
///
/// Webhooks can arrive late or out of order. A failed invoice that has since been paid becomes `invoice.paid`;
/// a voided one is dropped (`None`). Without a client the event is used as sent.
pub async fn latest_event(
    stripe: &Stripe,
    event: BillingEvent,
) -> Result<Option<BillingEvent>, StripeError> {
    if !stripe.is_enabled() {
        return Ok(Some(event));
    }
    match event {
        BillingEvent::Subscription {
            id,
            kind,
            subscription,
        } if kind != "customer.subscription.deleted" => {
            let live = stripe.subscription(&subscription.id).await?;
            Ok(Some(BillingEvent::Subscription {
                id,
                kind,
                subscription: subscription_snapshot(&live),
            }))
        }
        BillingEvent::Invoice {
            id,
            paid: false,
            invoice,
        } => {
            let live = stripe.invoice(&invoice.id).await?;
            match live["status"].as_str() {
                Some("void") => Ok(None),
                status => Ok(Some(BillingEvent::Invoice {
                    id,
                    paid: status == Some("paid"),
                    invoice: invoice_snapshot(&live),
                })),
            }
        }
        other => Ok(Some(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn signatures_are_checked_like_the_sdk() {
        let payload = br#"{"id":"evt_test"}"#;
        let header = signature_header(payload, "whsec_test_secret", 1_000);
        assert_eq!(
            verify_signature(payload, &header, "whsec_test_secret", 1_100),
            Ok(())
        );
        assert_eq!(
            verify_signature(payload, &header, "whsec_other", 1_100),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            verify_signature(b"{}", &header, "whsec_test_secret", 1_100),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            verify_signature(payload, &header, "whsec_test_secret", 1_301),
            Err(SignatureError::TooOld)
        );
        assert_eq!(
            verify_signature(payload, "v1=abc", "whsec_test_secret", 1_000),
            Err(SignatureError::Malformed)
        );
        // Several v1 signatures (secret rotation): any may match.
        let rotated = format!("t=1000,v1=00,{}", header.split_once(',').unwrap().1);
        assert_eq!(
            verify_signature(payload, &rotated, "whsec_test_secret", 1_000),
            Ok(())
        );
    }

    #[test]
    fn subscriptions_read_their_first_item() {
        let snapshot = subscription_snapshot(&json!({
            "id": "sub_1", "object": "subscription", "status": "active",
            "customer": { "id": "cus_1", "object": "customer" },
            "default_payment_method": "pm_1",
            "metadata": { "company_id": "100" },
            "items": { "data": [{
                "id": "si_1", "quantity": 3,
                "current_period_start": 1_788_220_800, "current_period_end": 1_790_812_800,
                "price": { "id": "price_monthly", "nickname": "", "unit_amount": 4900 },
                "plan": { "nickname": "Granite Manager" },
            }] },
        }));
        assert_eq!(snapshot.customer_id, "cus_1");
        assert_eq!(snapshot.plan_name.as_deref(), Some("Granite Manager"));
        assert_eq!(snapshot.quantity, Some(3));
        assert_eq!(snapshot.unit_amount_cents, Some(4900));
        assert_eq!(snapshot.company_id, Some(100));
        assert_eq!(
            snapshot.period_start.unwrap().to_string(),
            "2026-09-01 00:00:00"
        );
    }

    #[test]
    fn events_billing_ignores_are_none() {
        assert_eq!(
            normalize_event(
                &json!({ "id": "evt", "type": "charge.succeeded", "data": { "object": { "object": "charge" } } })
            ),
            None
        );
        // A checkout without a company cannot be placed.
        assert_eq!(
            normalize_event(&json!({ "id": "evt", "type": "checkout.session.completed",
                "data": { "object": { "object": "checkout.session", "customer": "cus_1", "metadata": {} } } })),
            None
        );
        assert_eq!(
            normalize_event(&json!({ "id": "evt", "type": "checkout.session.completed",
                "data": { "object": { "object": "checkout.session", "customer": "cus_1",
                    "client_reference_id": "7", "subscription": "sub_1" } } })),
            Some(BillingEvent::CheckoutCompleted {
                id: "evt".into(),
                company_id: 7,
                customer_id: "cus_1".into(),
                subscription_id: Some("sub_1".into()),
            })
        );
    }

    #[test]
    fn metadata_ids_are_positive_whole_numbers() {
        assert_eq!(positive_int(&json!("12")), Some(12));
        assert_eq!(positive_int(&json!(" 12 ")), Some(12));
        assert_eq!(positive_int(&json!("0")), None);
        assert_eq!(positive_int(&json!("1.5")), None);
        assert_eq!(positive_int(&json!("abc")), None);
        assert_eq!(positive_int(&json!(12)), None);
    }
}
