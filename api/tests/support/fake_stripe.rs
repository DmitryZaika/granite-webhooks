//! A stand-in for the Stripe API for the signup, provisioning, seat and plan
//! invite tests.
//!
//! It keeps customers, SetupIntents and subscriptions (with their items) in
//! memory, answers a repeated `Idempotency-Key` with the first response (and a
//! changed request under a used key with Stripe's 400 `idempotency_error`), and
//! can fail the next matching requests: before doing anything (an outage), or
//! after doing the work (the answer was lost). Also: subscription updates
//! (`cancel_at_period_end`, `default_payment_method`), cancelling one
//! (`DELETE /v1/subscriptions/{id}`), invoice previews
//! (`POST /v1/invoices/create_preview`: the items' monthly sum, or 404 when the
//! subscription ends at its period end), `GET /v1/invoices` from `invoices`,
//! and `payment_methods`, which a customer's default expands into. Payment
//! methods (step 13): SetupIntent cancel and micro-deposit states
//! (`pend_microdeposits`, `fail_setup_intent`), payment method detach,
//! `POST /v1/invoices/{id}/pay` (paid, a bank debit processing, a decline,
//! 3D Secure), charges and PaymentIntents by id. The onboarding advance:
//! invoice items (`POST`, `GET`, list with `pending`, `DELETE` while pending
//! or on a draft), [`FakeStripe::sweep_items`] (the renewal invoice taking
//! the pending items), customer balance transactions, and `cancel_at` on a
//! subscription (an empty value removes it). Include it with
//! `#[path = "../support/fake_stripe.rs"] mod fake_stripe;` in the area's `main.rs`.
#![allow(dead_code)]

use api::billing::stripe::Stripe;
use axum::Json;
use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::routing::get;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One request the fake received.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    /// Form body (POST, DELETE) or query string (GET).
    pub body: String,
    pub idempotency_key: Option<String>,
    /// Answered from the idempotency cache.
    pub replayed: bool,
}

impl Seen {
    /// The decoded form fields, repeated names kept in order.
    pub fn fields(&self) -> Vec<(String, String)> {
        pairs(&self.body)
    }

    pub fn field(&self, name: &str) -> Option<String> {
        self.fields()
            .into_iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }
}

/// When an injected failure happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailMode {
    /// 500 before anything changes (an outage).
    Before,
    /// The work is done and cached under its idempotency key, then 500 (the
    /// answer was lost on the way back).
    After,
}

#[derive(Debug, Clone)]
struct Failure {
    method: Method,
    path_prefix: String,
    mode: FailMode,
    times: u32,
}

#[derive(Default)]
pub struct FakeStripe {
    /// Price objects, answered by `GET /v1/prices?lookup_keys[]=`.
    pub prices: Vec<Value>,
    pub customers: HashMap<String, Value>,
    pub setup_intents: HashMap<String, Value>,
    pub subscriptions: HashMap<String, Value>,
    /// Invoice objects, answered by `GET /v1/invoices` (any customer).
    pub invoices: Vec<Value>,
    /// Payment method objects by id, expanded into a customer's
    /// `invoice_settings.default_payment_method` and a subscription's
    /// `default_payment_method`.
    pub payment_methods: HashMap<String, Value>,
    /// Tax added to each invoice preview, in cents.
    pub preview_tax: i64,
    /// Automatic tax on each line [`FakeStripe::sweep_items`] makes, in
    /// percent (dahlia's exclusive `taxes[]`); 0: no tax.
    pub line_tax_percent: i64,
    /// Charge objects by id (`GET /v1/charges/{id}`).
    pub charges: HashMap<String, Value>,
    /// PaymentIntent objects by id (`GET /v1/payment_intents/{id}`).
    pub payment_intents: HashMap<String, Value>,
    /// Invoice item objects (`/v1/invoiceitems`); a deleted one is removed.
    pub invoice_items: Vec<Value>,
    /// Customer balance transactions (`/v1/customers/{id}/balance_transactions`).
    pub balance_transactions: Vec<Value>,
    /// The next `POST /v1/invoices/{id}/pay` is declined (402 `card_error`).
    pub decline_next_pay: bool,
    /// Invoices whose payment needs 3D Secure: paying answers 402
    /// `invoice_payment_intent_requires_action`.
    pub requires_action_invoices: Vec<String>,
    /// Webhook endpoints, portal configurations and payment method
    /// configurations (the setup script and the setup check).
    pub webhook_endpoints: Vec<Value>,
    pub portal_configurations: Vec<Value>,
    pub payment_method_configurations: Vec<Value>,
    pub requests: Vec<Seen>,
    idempotent: HashMap<String, (String, String, StatusCode, Value)>,
    failures: Vec<Failure>,
    next_id: u32,
}

pub type Shared = Arc<Mutex<FakeStripe>>;

/// The five catalog prices of 03-stripe.md section 4.1.
pub fn catalog_prices() -> Vec<Value> {
    [
        ("price_base", "granite_base_monthly_v1", 30_000),
        ("price_extra_user", "granite_extra_user_monthly_v1", 3_000),
        ("price_signatures", "addon_signatures_monthly_v1", 1_500),
        ("price_quotes", "addon_quotes_monthly_v1", 7_500),
        ("price_quickbooks", "addon_quickbooks_monthly_v1", 3_000),
    ]
    .into_iter()
    .map(|(id, key, cents)| {
        let mut price = price(id, key, cents);
        price["metadata"] = match key {
            "granite_base_monthly_v1" => json!({ "kind": "base" }),
            "granite_extra_user_monthly_v1" => json!({ "kind": "seat" }),
            _ => json!({ "addon_key": key.split('_').nth(1) }),
        };
        price
    })
    .collect()
}

/// A card payment method.
pub fn card(id: &str, brand: &str, last4: &str) -> Value {
    json!({
        "id": id, "object": "payment_method", "type": "card",
        "card": { "brand": brand, "last4": last4, "exp_month": 12, "exp_year": 2030 },
    })
}

/// A US bank account payment method.
pub fn bank_account(id: &str, bank_name: &str, last4: &str) -> Value {
    json!({
        "id": id, "object": "payment_method", "type": "us_bank_account",
        "us_bank_account": { "bank_name": bank_name, "last4": last4, "account_type": "checking" },
    })
}

/// An active monthly USD per-unit price.
pub fn price(id: &str, lookup_key: &str, unit_amount: i64) -> Value {
    json!({
        "id": id, "object": "price", "active": true, "currency": "usd",
        "lookup_key": lookup_key, "unit_amount": unit_amount, "billing_scheme": "per_unit",
        "type": "recurring", "recurring": { "interval": "month", "interval_count": 1 },
        "livemode": false, "nickname": null, "tax_behavior": "exclusive",
        "metadata": {},
    })
}

impl FakeStripe {
    /// A fake with the catalog prices.
    pub fn with_catalog() -> Shared {
        Arc::new(Mutex::new(Self {
            prices: catalog_prices(),
            ..Self::default()
        }))
    }

    /// The next `times` requests whose path starts with `path_prefix` fail.
    pub fn fail(&mut self, method: Method, path_prefix: &str, mode: FailMode, times: u32) {
        self.failures.push(Failure {
            method,
            path_prefix: path_prefix.to_string(),
            mode,
            times,
        });
    }

    /// Requests (not replays) with this method and exact path.
    pub fn calls(&self, method: &str, path: &str) -> Vec<Seen> {
        self.requests
            .iter()
            .filter(|seen| seen.method == method && seen.path == path && !seen.replayed)
            .cloned()
            .collect()
    }

    /// The browser's `stripe.confirmSetup`: the SetupIntent succeeds with a card.
    pub fn confirm_setup_intent(&mut self, id: &str, payment_method: &str) {
        let intent = self.setup_intents.get_mut(id).expect("setup intent");
        intent["status"] = json!("succeeded");
        intent["payment_method"] = json!(payment_method);
    }

    /// Micro-deposits sent: the SetupIntent waits (`requires_action`,
    /// `verify_with_microdeposits`) with `payment_method` attached.
    pub fn pend_microdeposits(&mut self, setup_intent: &str, payment_method: &str) {
        let intent = self
            .setup_intents
            .get_mut(setup_intent)
            .expect("setup intent");
        intent["status"] = json!("requires_action");
        intent["payment_method"] = json!(payment_method);
        intent["next_action"] = json!({
            "type": "verify_with_microdeposits",
            "verify_with_microdeposits": {
                "arrival_date": 1_800_086_400,
                "hosted_verification_url": "https://payments.stripe.com/microdeposit/fake",
                "microdeposit_type": "descriptor_code",
            },
        });
    }

    /// The SetupIntent failed (`requires_payment_method`, `last_setup_error.code`).
    pub fn fail_setup_intent(&mut self, setup_intent: &str, code: &str) {
        let intent = self
            .setup_intents
            .get_mut(setup_intent)
            .expect("setup intent");
        intent["status"] = json!("requires_payment_method");
        intent["next_action"] = Value::Null;
        intent["last_setup_error"] = json!({ "code": code, "message": "fake failure" });
    }

    /// Stripe verified the micro-deposits: the SetupIntent succeeded.
    pub fn verify_microdeposits(&mut self, setup_intent: &str) {
        let intent = self
            .setup_intents
            .get_mut(setup_intent)
            .expect("setup intent");
        intent["status"] = json!("succeeded");
        intent["next_action"] = Value::Null;
    }

    /// Stripe's renewal of `subscription`: a new invoice `invoice_id` in
    /// `status` with a line per pending item of it (dahlia's
    /// `parent.invoice_item_details`, the item's metadata), the items then on
    /// that invoice. Answers the invoice (also in `invoices`).
    pub fn sweep_items(&mut self, subscription: &str, invoice_id: &str, status: &str) -> Value {
        let mut lines = Vec::new();
        let mut customer = Value::Null;
        let mut tax = 0;
        for item in &mut self.invoice_items {
            if item["subscription"] == subscription && item["invoice"].is_null() {
                item["invoice"] = json!(invoice_id);
                customer = item["customer"].clone();
                let mut line = json!({
                    "id": format!("il_{}", item["id"].as_str().unwrap_or_default()),
                    "object": "line_item", "amount": item["amount"],
                    "description": item["description"], "metadata": item["metadata"],
                    "parent": { "type": "invoice_item_details",
                        "invoice_item_details": { "invoice_item": item["id"] } },
                });
                if self.line_tax_percent > 0 {
                    let line_tax =
                        item["amount"].as_i64().unwrap_or_default() * self.line_tax_percent / 100;
                    tax += line_tax;
                    line["taxes"] = json!([{ "amount": line_tax, "tax_behavior": "exclusive",
                        "taxability_reason": "standard_rated", "type": "tax_rate_details" }]);
                }
                lines.push(line);
            }
        }
        let total: i64 =
            lines.iter().filter_map(|line| line["amount"].as_i64()).sum::<i64>() + tax;
        let invoice = json!({
            "id": invoice_id, "object": "invoice", "customer": customer, "status": status,
            "amount_due": total, "amount_paid": if status == "paid" { total } else { 0 },
            "currency": "usd",
            "parent": { "type": "subscription_details",
                "subscription_details": { "subscription": subscription,
                    "metadata": self.subscriptions.get(subscription)
                        .map_or(Value::Null, |sub| sub["metadata"].clone()) } },
            "lines": { "object": "list", "data": lines, "has_more": false },
        });
        self.invoices.push(invoice.clone());
        invoice
    }

    /// The idempotency keys of the requests (not replays) to `path`.
    pub fn keys(&self, method: &str, path: &str) -> Vec<String> {
        self.calls(method, path)
            .into_iter()
            .filter_map(|seen| seen.idempotency_key)
            .collect()
    }

    fn id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}_{}", self.next_id)
    }

    fn take_failure(&mut self, method: &Method, path: &str) -> Option<FailMode> {
        let failure = self
            .failures
            .iter_mut()
            .find(|f| f.times > 0 && f.method == *method && path.starts_with(&f.path_prefix))?;
        failure.times -= 1;
        Some(failure.mode)
    }

    fn price_object(&self, id: &str) -> Value {
        self.prices
            .iter()
            .find(|price| price["id"] == id)
            .cloned()
            .unwrap_or_else(|| price(id, "", 0))
    }

    /// Adds a subscription item to the fake (tests seeding a subscription).
    pub fn item(&mut self, price_id: &str, quantity: i64, subscription: &str) -> Value {
        let id = self.id("si");
        json!({
            "id": id, "object": "subscription_item", "subscription": subscription,
            "quantity": quantity, "price": self.price_object(price_id),
            "current_period_start": 1_798_000_000, "current_period_end": 1_800_600_000,
        })
    }

    fn route(&mut self, method: &Method, path: &str, body: &str) -> (StatusCode, Value) {
        let params = pairs(body);
        let one = |name: &str| {
            params
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        if let Some(answer) = self.setup_route(method, &segments, &params) {
            return answer;
        }
        match (method.as_str(), segments.as_slice()) {
            ("GET", ["v1", "prices"]) => {
                let keys: Vec<String> = params
                    .iter()
                    .filter(|(key, _)| key == "lookup_keys[]")
                    .map(|(_, value)| value.clone())
                    .collect();
                let data: Vec<Value> = self
                    .prices
                    .iter()
                    .filter(|price| {
                        price["active"] == true
                            && keys.iter().any(|key| price["lookup_key"] == key.as_str())
                    })
                    .cloned()
                    .collect();
                ok(json!({ "object": "list", "data": data }))
            }
            ("POST", ["v1", "customers"]) => {
                let id = self.id("cus");
                let customer = json!({
                    "id": id, "object": "customer", "email": one("email"), "name": one("name"),
                    "phone": one("phone"),
                    "metadata": metadata(&params),
                    "invoice_settings": { "default_payment_method": null },
                });
                self.customers.insert(id, customer.clone());
                ok(customer)
            }
            ("GET", ["v1", "customers", id]) => match self.customers.get(*id) {
                Some(customer) => {
                    let mut customer = customer.clone();
                    let default = &customer["invoice_settings"]["default_payment_method"];
                    if let Some(method) =
                        default.as_str().and_then(|id| self.payment_methods.get(id))
                    {
                        customer["invoice_settings"]["default_payment_method"] = method.clone();
                    }
                    ok(customer)
                }
                None => missing(),
            },
            ("POST", ["v1", "customers", id]) => {
                let Some(customer) = self.customers.get_mut(*id) else {
                    return missing();
                };
                for (key, value) in metadata(&params).as_object().into_iter().flatten() {
                    customer["metadata"][key] = value.clone();
                }
                if let Some(method) = one("invoice_settings[default_payment_method]") {
                    customer["invoice_settings"]["default_payment_method"] = json!(method);
                }
                ok(customer.clone())
            }
            ("DELETE", ["v1", "customers", id]) => match self.customers.remove(*id) {
                Some(_) => ok(json!({ "id": id, "object": "customer", "deleted": true })),
                None => missing(),
            },
            ("POST", ["v1", "setup_intents"]) => {
                let id = self.id("seti");
                let confirmed = one("confirm").as_deref() == Some("true");
                let intent = json!({
                    "id": id, "object": "setup_intent", "customer": one("customer"),
                    "client_secret": format!("{id}_secret_fake"),
                    "status": if confirmed { "succeeded" } else { "requires_payment_method" },
                    "payment_method": if confirmed { json!(one("payment_method")) } else { Value::Null },
                    "usage": one("usage"), "metadata": metadata(&params),
                    "created": 1_800_000_000, "next_action": null,
                    "payment_method_types": params.iter()
                        .filter(|(key, _)| key == "payment_method_types[]")
                        .map(|(_, value)| value.clone()).collect::<Vec<_>>(),
                });
                self.setup_intents.insert(id, intent.clone());
                ok(intent)
            }
            ("POST", ["v1", "setup_intents", id, "cancel"]) => {
                let Some(intent) = self.setup_intents.get_mut(*id) else {
                    return missing();
                };
                intent["status"] = json!("canceled");
                intent["next_action"] = Value::Null;
                ok(intent.clone())
            }
            ("POST", ["v1", "payment_methods", id, "detach"]) => {
                match self.payment_methods.get_mut(*id) {
                    Some(method) => {
                        method["customer"] = Value::Null;
                        ok(method.clone())
                    }
                    None => missing(),
                }
            }
            ("GET", ["v1", "charges", id]) => match self.charges.get(*id) {
                Some(charge) => ok(charge.clone()),
                None => missing(),
            },
            ("GET", ["v1", "payment_intents", id]) => match self.payment_intents.get(*id) {
                Some(intent) => ok(intent.clone()),
                None => missing(),
            },
            ("GET", ["v1", "invoices", id]) => {
                match self.invoices.iter().find(|invoice| invoice["id"] == *id) {
                    Some(invoice) => ok(invoice.clone()),
                    None => missing(),
                }
            }
            ("POST", ["v1", "invoices", id, "pay"]) => self.pay(id, one("payment_method")),
            ("GET", ["v1", "setup_intents", id]) => {
                let Some(mut intent) = self.setup_intents.get(*id).cloned() else {
                    return missing();
                };
                if let Some(method) = intent["payment_method"].as_str().map(str::to_string) {
                    intent["payment_method"] = self
                        .payment_methods
                        .get(&method)
                        .cloned()
                        .unwrap_or_else(|| {
                            json!({
                                "id": method, "object": "payment_method", "type": "card",
                                "card": { "brand": "visa", "last4": "4242" },
                            })
                        });
                }
                ok(intent)
            }
            ("POST", ["v1", "subscriptions"]) => {
                let id = self.id("sub");
                let mut items = Vec::new();
                for index in 0.. {
                    let Some(price) = one(&format!("items[{index}][price]")) else {
                        break;
                    };
                    let quantity = one(&format!("items[{index}][quantity]"))
                        .and_then(|q| q.parse().ok())
                        .unwrap_or(1);
                    items.push(self.item(&price, quantity, &id));
                }
                // `trial_end=now`: the trial is over at once (active, no trial_end).
                let trial_end = one("trial_end").and_then(|t| t.parse::<i64>().ok());
                let subscription = json!({
                    "id": id, "object": "subscription", "customer": one("customer"),
                    "status": if trial_end.is_some() { "trialing" } else { "active" },
                    "trial_end": trial_end,
                    "default_payment_method": one("default_payment_method"),
                    "metadata": metadata(&params),
                    "items": { "object": "list", "data": items },
                });
                self.subscriptions.insert(id, subscription.clone());
                ok(subscription)
            }
            ("GET", ["v1", "subscriptions"]) => {
                let customer = one("customer").unwrap_or_default();
                let data: Vec<Value> = self
                    .subscriptions
                    .values()
                    .filter(|sub| sub["customer"] == customer.as_str())
                    .cloned()
                    .collect();
                ok(json!({ "object": "list", "data": data }))
            }
            ("GET", ["v1", "subscriptions", id]) => match self.subscriptions.get(*id) {
                Some(subscription) => {
                    let mut subscription = subscription.clone();
                    let default = &subscription["default_payment_method"];
                    if let Some(method) =
                        default.as_str().and_then(|id| self.payment_methods.get(id))
                    {
                        subscription["default_payment_method"] = method.clone();
                    }
                    ok(subscription)
                }
                None => missing(),
            },
            ("DELETE", ["v1", "subscriptions", id]) => match self.subscriptions.get_mut(*id) {
                Some(subscription) => {
                    subscription["status"] = json!("canceled");
                    ok(subscription.clone())
                }
                None => missing(),
            },
            ("POST", ["v1", "subscriptions", id]) => {
                let Some(subscription) = self.subscriptions.get_mut(*id) else {
                    return missing();
                };
                for (key, value) in metadata(&params).as_object().into_iter().flatten() {
                    subscription["metadata"][key] = value.clone();
                }
                if let Some(cancel) = one("cancel_at_period_end") {
                    let cancel = cancel == "true";
                    subscription["cancel_at_period_end"] = json!(cancel);
                    let end = if subscription["status"] == "trialing" {
                        subscription["trial_end"].clone()
                    } else {
                        subscription["items"]["data"][0]["current_period_end"].clone()
                    };
                    subscription["cancel_at"] = if cancel { end } else { Value::Null };
                    subscription["cancellation_details"]["comment"] =
                        one("cancellation_details[comment]").map_or(Value::Null, |c| json!(c));
                }
                if let Some(at) = one("cancel_at") {
                    subscription["cancel_at_period_end"] = json!(false);
                    subscription["cancel_at"] = at.parse::<i64>().map_or(Value::Null, |at| json!(at));
                    if let Some(comment) = one("cancellation_details[comment]") {
                        subscription["cancellation_details"]["comment"] = json!(comment);
                    }
                }
                if let Some(method) = one("default_payment_method") {
                    subscription["default_payment_method"] = json!(method);
                }
                ok(subscription.clone())
            }
            ("POST", ["v1", "invoices", "create_preview"]) => {
                let id = one("subscription").unwrap_or_default();
                let Some(subscription) = self.subscriptions.get(&id) else {
                    return missing();
                };
                if subscription["cancel_at_period_end"] == true {
                    return (
                        StatusCode::NOT_FOUND,
                        json!({ "error": { "type": "invalid_request_error",
                            "code": "invoice_upcoming_none",
                            "message": "No upcoming invoices for customer" } }),
                    );
                }
                let items = subscription["items"]["data"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let lines: Vec<Value> = items
                    .iter()
                    .map(|item| {
                        let quantity = item["quantity"].as_i64().unwrap_or(1);
                        let amount = item["price"]["unit_amount"].as_i64().unwrap_or(0) * quantity;
                        json!({ "object": "line_item", "amount": amount, "quantity": quantity,
                            "description": format!("{quantity} x {}", item["price"]["lookup_key"].as_str().unwrap_or("item")),
                            "pricing": { "price_details": { "price": item["price"]["id"] } } })
                    })
                    .collect();
                let subtotal: i64 = lines
                    .iter()
                    .filter_map(|line| line["amount"].as_i64())
                    .sum();
                let total = subtotal + self.preview_tax;
                let date = if subscription["status"] == "trialing" {
                    subscription["trial_end"].clone()
                } else {
                    items
                        .first()
                        .map_or(Value::Null, |item| item["current_period_end"].clone())
                };
                ok(json!({
                    "object": "invoice", "customer": subscription["customer"], "subscription": id,
                    "subtotal": subtotal, "total": total, "amount_due": total,
                    "total_excluding_tax": subtotal,
                    "total_taxes": [{ "amount": self.preview_tax }],
                    "period_end": date, "next_payment_attempt": date, "currency": "usd",
                    "lines": { "object": "list", "data": lines },
                }))
            }
            ("GET", ["v1", "invoices"]) => {
                let customer = one("customer");
                let status = one("status");
                let data: Vec<Value> = self
                    .invoices
                    .iter()
                    .filter(|invoice| customer.as_deref().is_none_or(|c| invoice["customer"] == c))
                    .filter(|invoice| status.as_deref().is_none_or(|s| invoice["status"] == s))
                    .cloned()
                    .collect();
                ok(json!({ "object": "list", "data": data, "has_more": false }))
            }
            ("GET", ["v1", "payment_methods", id]) => match self.payment_methods.get(*id) {
                Some(method) => ok(method.clone()),
                None => missing(),
            },
            ("POST", ["v1", "invoiceitems"]) => {
                let id = self.id("ii");
                let item = json!({
                    "id": id, "object": "invoiceitem", "customer": one("customer"),
                    "subscription": one("subscription"),
                    "amount": one("amount").and_then(|a| a.parse::<i64>().ok()),
                    "currency": one("currency"), "description": one("description"),
                    "metadata": metadata(&params), "invoice": null,
                });
                self.invoice_items.push(item.clone());
                ok(item)
            }
            ("GET", ["v1", "invoiceitems"]) => {
                let customer = one("customer");
                let pending = one("pending").as_deref() == Some("true");
                let data: Vec<Value> = self
                    .invoice_items
                    .iter()
                    .filter(|item| customer.as_deref().is_none_or(|c| item["customer"] == c))
                    .filter(|item| !pending || item["invoice"].is_null())
                    .cloned()
                    .collect();
                ok(json!({ "object": "list", "data": data, "has_more": false }))
            }
            ("GET", ["v1", "invoiceitems", id]) => {
                match self.invoice_items.iter().find(|item| item["id"] == *id) {
                    Some(item) => ok(item.clone()),
                    None => missing(),
                }
            }
            ("DELETE", ["v1", "invoiceitems", id]) => {
                let Some(index) = self.invoice_items.iter().position(|item| item["id"] == *id)
                else {
                    return missing();
                };
                let on = self.invoice_items[index]["invoice"].as_str().map(str::to_string);
                let draft = on.as_ref().is_none_or(|invoice| {
                    self.invoices
                        .iter()
                        .any(|i| i["id"] == invoice.as_str() && i["status"] == "draft")
                });
                if !draft {
                    return (
                        StatusCode::BAD_REQUEST,
                        json!({ "error": { "type": "invalid_request_error",
                            "message": "This invoice item is on a finalized invoice" } }),
                    );
                }
                self.invoice_items.remove(index);
                ok(json!({ "id": id, "object": "invoiceitem", "deleted": true }))
            }
            ("POST", ["v1", "customers", customer, "balance_transactions"]) => {
                let id = self.id("cbtxn");
                let txn = json!({
                    "id": id, "object": "customer_balance_transaction", "customer": customer,
                    "amount": one("amount").and_then(|a| a.parse::<i64>().ok()),
                    "currency": one("currency"), "description": one("description"),
                    "metadata": metadata(&params),
                });
                self.balance_transactions.push(txn.clone());
                ok(txn)
            }
            ("GET", ["v1", "customers", customer, "balance_transactions"]) => {
                let data: Vec<Value> = self
                    .balance_transactions
                    .iter()
                    .filter(|txn| txn["customer"] == *customer)
                    .cloned()
                    .collect();
                ok(json!({ "object": "list", "data": data, "has_more": false }))
            }
            ("POST", ["v1", "subscription_items"]) => {
                let subscription = one("subscription").unwrap_or_default();
                if !self.subscriptions.contains_key(&subscription) {
                    return missing();
                }
                let quantity = one("quantity").and_then(|q| q.parse().ok()).unwrap_or(1);
                let item = self.item(&one("price").unwrap_or_default(), quantity, &subscription);
                if let Some(Value::Array(items)) = self
                    .subscriptions
                    .get_mut(&subscription)
                    .map(|sub| &mut sub["items"]["data"])
                {
                    items.push(item.clone());
                }
                ok(item)
            }
            ("POST", ["v1", "subscription_items", id]) => {
                let quantity: Option<i64> = one("quantity").and_then(|q| q.parse().ok());
                for subscription in self.subscriptions.values_mut() {
                    if let Some(Value::Array(items)) = subscription
                        .get_mut("items")
                        .and_then(|i| i.get_mut("data"))
                        && let Some(item) = items.iter_mut().find(|item| item["id"] == *id)
                    {
                        if let Some(quantity) = quantity {
                            item["quantity"] = json!(quantity);
                        }
                        return ok(item.clone());
                    }
                }
                missing()
            }
            ("DELETE", ["v1", "subscription_items", id]) => {
                for subscription in self.subscriptions.values_mut() {
                    if let Some(Value::Array(items)) = subscription
                        .get_mut("items")
                        .and_then(|i| i.get_mut("data"))
                        && let Some(index) = items.iter().position(|item| item["id"] == *id)
                    {
                        items.remove(index);
                        return ok(
                            json!({ "id": id, "object": "subscription_item", "deleted": true }),
                        );
                    }
                }
                missing()
            }
            _ => (
                StatusCode::NOT_FOUND,
                json!({ "error": { "message": format!("fake Stripe has no {method} {path}") } }),
            ),
        }
    }

    /// `POST /v1/invoices/{id}/pay`: declined (`decline_next_pay`), 3D Secure
    /// (`requires_action_invoices`), processing for a bank account (the
    /// invoice stays open, its PaymentIntent `processing`), else paid.
    fn pay(&mut self, id: &str, payment_method: Option<String>) -> (StatusCode, Value) {
        let Some(index) = self.invoices.iter().position(|invoice| invoice["id"] == id) else {
            return missing();
        };
        if self.invoices[index]["status"] != "open" {
            return (
                StatusCode::BAD_REQUEST,
                json!({ "error": { "type": "invalid_request_error", "code": "invoice_not_open",
                    "message": "Invoice is already paid" } }),
            );
        }
        if self.decline_next_pay {
            self.decline_next_pay = false;
            return (
                StatusCode::PAYMENT_REQUIRED,
                json!({ "error": { "type": "card_error", "code": "card_declined",
                    "decline_code": "generic_decline", "message": "Your card was declined." } }),
            );
        }
        if self
            .requires_action_invoices
            .iter()
            .any(|invoice| invoice == id)
        {
            self.invoices[index]["confirmation_secret"] = json!({ "client_secret": format!("pi_{id}_secret_fake"), "type": "payment_intent" });
            return (
                StatusCode::PAYMENT_REQUIRED,
                json!({ "error": { "type": "invalid_request_error",
                    "code": "invoice_payment_intent_requires_action",
                    "message": "This payment requires additional user action before it can be completed successfully." } }),
            );
        }
        let bank = payment_method
            .as_deref()
            .and_then(|method| self.payment_methods.get(method))
            .is_some_and(|method| method["type"] == "us_bank_account");
        let invoice = &mut self.invoices[index];
        if bank {
            let intent = format!("pi_{id}");
            let customer = invoice["customer"].clone();
            let amount = invoice["amount_remaining"].clone();
            self.payment_intents.insert(
                intent.clone(),
                json!({ "id": intent, "object": "payment_intent", "status": "processing",
                    "customer": customer, "amount": amount,
                    "payment_method_types": ["card", "us_bank_account"] }),
            );
            return ok(self.invoices[index].clone());
        }
        let due = invoice["amount_remaining"]
            .as_i64()
            .or_else(|| invoice["amount_due"].as_i64())
            .unwrap_or_default();
        invoice["status"] = json!("paid");
        invoice["amount_paid"] = json!(due);
        invoice["amount_remaining"] = json!(0);
        ok(invoice.clone())
    }

    fn handle(
        &mut self,
        method: &Method,
        path: &str,
        body: String,
        key: Option<String>,
    ) -> (StatusCode, Value) {
        let failure = self.take_failure(method, path);
        if let Some(key) = &key
            && let Some((first_path, first_body, status, response)) = self.idempotent.get(key)
        {
            let same = first_path == path && first_body == &body;
            let answer = if same {
                (*status, response.clone())
            } else {
                (
                    StatusCode::BAD_REQUEST,
                    json!({ "error": { "type": "idempotency_error",
                        "message": "Keys for idempotent requests can only be used with the same parameters they were first used with." } }),
                )
            };
            self.requests.push(Seen {
                method: method.to_string(),
                path: path.to_string(),
                body,
                idempotency_key: Some(key.clone()),
                replayed: true,
            });
            return answer;
        }
        self.requests.push(Seen {
            method: method.to_string(),
            path: path.to_string(),
            body: body.clone(),
            idempotency_key: key.clone(),
            replayed: false,
        });
        if failure == Some(FailMode::Before) {
            return outage();
        }
        let (status, response) = self.route(method, path, &body);
        if let Some(key) = key
            && *method == Method::POST
        {
            self.idempotent
                .insert(key, (path.to_string(), body, status, response.clone()));
        }
        if failure == Some(FailMode::After) {
            return outage();
        }
        (status, response)
    }
}

/// Sets `a[b][]=x`, `a[b][0]=x` style form fields into a JSON object;
/// `true`/`false` become booleans.
fn set_path(target: &mut Value, key: &str, value: &str) {
    let mut segments: Vec<String> = Vec::new();
    for (index, part) in key.split('[').enumerate() {
        segments.push(if index == 0 {
            part.to_string()
        } else {
            part.trim_end_matches(']').to_string()
        });
    }
    let value = match value {
        "true" => json!(true),
        "false" => json!(false),
        other => json!(other),
    };
    let mut node = target;
    for (position, segment) in segments.iter().enumerate() {
        let last = position + 1 == segments.len();
        let next_is_list = segments
            .get(position + 1)
            .is_some_and(|next| next.is_empty() || next.chars().all(|c| c.is_ascii_digit()));
        if last {
            if let Value::Array(list) = node {
                list.push(value);
            } else {
                node[segment] = value;
            }
            return;
        }
        if segment.is_empty() || segment.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if node[segment].is_null() {
            node[segment] = if next_is_list { json!([]) } else { json!({}) };
        }
        node = &mut node[segment];
    }
}

impl FakeStripe {
    /// The account setup endpoints: `GET /v1/balance`, products inline in
    /// `POST /v1/prices`, portal configurations, webhook endpoints (the
    /// signing secret is answered on creation only) and
    /// `GET /v1/payment_method_configurations`.
    fn setup_route(
        &mut self,
        method: &Method,
        segments: &[&str],
        params: &[(String, String)],
    ) -> Option<(StatusCode, Value)> {
        let list = |items: &[Value]| ok(json!({ "object": "list", "data": items }));
        let events = || -> Vec<Value> {
            params
                .iter()
                .filter(|(key, _)| key == "enabled_events[]")
                .map(|(_, value)| json!(value))
                .collect()
        };
        Some(match (method.as_str(), segments) {
            ("GET", ["v1", "balance"]) => ok(json!({ "object": "balance", "livemode": false })),
            ("GET", ["v1", "payment_method_configurations"]) => {
                list(&self.payment_method_configurations)
            }
            ("GET", ["v1", "webhook_endpoints"]) => list(&self.webhook_endpoints),
            ("GET", ["v1", "billing_portal", "configurations"]) => {
                let active: Vec<Value> = self
                    .portal_configurations
                    .iter()
                    .filter(|config| config["active"] != false)
                    .cloned()
                    .collect();
                list(&active)
            }
            ("POST", ["v1", "prices"]) => {
                let one = |name: &str| {
                    params
                        .iter()
                        .find(|(key, _)| key == name)
                        .map(|(_, value)| value.clone())
                };
                let id = self.id("price");
                let price = json!({
                    "id": id, "object": "price", "active": true, "livemode": false,
                    "currency": one("currency"), "lookup_key": one("lookup_key"),
                    "unit_amount": one("unit_amount").and_then(|v| v.parse::<i64>().ok()),
                    "billing_scheme": one("billing_scheme").unwrap_or_else(|| "per_unit".into()),
                    "tax_behavior": one("tax_behavior").unwrap_or_else(|| "unspecified".into()),
                    "type": "recurring",
                    "recurring": { "interval": one("recurring[interval]"), "interval_count": 1 },
                    "product": self.id("prod"),
                    "metadata": metadata(params),
                });
                self.prices.push(price.clone());
                ok(price)
            }
            ("POST", ["v1", "billing_portal", "configurations"]) => {
                let id = self.id("bpc");
                let mut config = json!({ "id": id, "object": "billing_portal.configuration",
                    "active": true, "livemode": false });
                for (key, value) in params {
                    set_path(&mut config, key, value);
                }
                self.portal_configurations.push(config.clone());
                ok(config)
            }
            ("POST", ["v1", "billing_portal", "configurations", id]) => {
                let Some(config) = self
                    .portal_configurations
                    .iter_mut()
                    .find(|config| config["id"] == *id)
                else {
                    return Some(missing());
                };
                let mut cleared: Vec<&str> = Vec::new();
                for (key, value) in params {
                    if let Some(base) = key.strip_suffix("[]")
                        && !cleared.contains(&base)
                    {
                        cleared.push(base);
                        let mut node = &mut *config;
                        for part in base.split(['[', ']']).filter(|part| !part.is_empty()) {
                            node = &mut node[part];
                        }
                        *node = json!([]);
                    }
                    set_path(config, key, value);
                }
                ok(config.clone())
            }
            ("POST", ["v1", "webhook_endpoints"]) => {
                let id = self.id("we");
                let url = params
                    .iter()
                    .find(|(key, _)| key == "url")
                    .map(|(_, value)| value.clone());
                let version = params
                    .iter()
                    .find(|(key, _)| key == "api_version")
                    .map(|(_, value)| value.clone());
                let endpoint = json!({ "id": id, "object": "webhook_endpoint", "url": url,
                    "api_version": version, "status": "enabled", "enabled_events": events(),
                    "livemode": false });
                self.webhook_endpoints.push(endpoint.clone());
                let mut created = endpoint;
                created["secret"] = json!(format!("whsec_fake_secret_{id}"));
                ok(created)
            }
            ("POST", ["v1", "webhook_endpoints", id]) => {
                let Some(endpoint) = self
                    .webhook_endpoints
                    .iter_mut()
                    .find(|endpoint| endpoint["id"] == *id)
                else {
                    return Some(missing());
                };
                endpoint["enabled_events"] = json!(events());
                ok(endpoint.clone())
            }
            _ => return None,
        })
    }
}

fn ok(value: Value) -> (StatusCode, Value) {
    (StatusCode::OK, value)
}

fn missing() -> (StatusCode, Value) {
    (
        StatusCode::NOT_FOUND,
        json!({ "error": { "type": "invalid_request_error", "code": "resource_missing",
            "message": "No such object" } }),
    )
}

fn outage() -> (StatusCode, Value) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "error": { "type": "api_error", "message": "fake outage" } }),
    )
}

/// `a=1&b[]=2&b[]=3` decoded, repeats kept.
pub fn pairs(encoded: &str) -> Vec<(String, String)> {
    serde_html_form::from_str::<Vec<(String, String)>>(encoded).unwrap_or_default()
}

fn metadata(params: &[(String, String)]) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in params {
        if let Some(name) = key
            .strip_prefix("metadata[")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            map.insert(name.to_string(), json!(value));
        }
    }
    Value::Object(map)
}

async fn any(
    State(shared): State<Shared>,
    method: Method,
    headers: HeaderMap,
    uri: axum::http::Uri,
    RawQuery(query): RawQuery,
    body: String,
) -> (StatusCode, Json<Value>) {
    let key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let input = if method == Method::GET {
        query.unwrap_or_default()
    } else {
        body
    };
    // Ids arrive percent-encoded (`seti%5F2`), as Stripe accepts them.
    let path = percent_encoding::percent_decode_str(uri.path()).decode_utf8_lossy();
    let (status, value) = shared.lock().unwrap().handle(&method, &path, input, key);
    (status, Json(value))
}

/// Serves the fake on a free port; answers its base URL.
pub async fn start(shared: Shared) -> String {
    let router = Router::new()
        .route("/{*path}", get(any).post(any).delete(any))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

/// A test-mode client for the fake.
pub async fn client(shared: &Shared) -> Stripe {
    let mut stripe = Stripe::new(Some("sk_test_fake"), &start(Arc::clone(shared)).await);
    stripe.poll_delay = std::time::Duration::from_millis(1);
    stripe
}
