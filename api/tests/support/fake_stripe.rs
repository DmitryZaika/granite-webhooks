//! A stand-in for the Stripe API for the signup, provisioning, seat and plan
//! invite tests.
//!
//! It keeps customers, SetupIntents and subscriptions (with their items) in
//! memory, answers a repeated `Idempotency-Key` with the first response (and a
//! changed request under a used key with Stripe's 400 `idempotency_error`), and
//! can fail the next matching requests: before doing anything (an outage), or
//! after doing the work (the answer was lost). Include it with
//! `#[path = "support/fake_stripe.rs"] mod fake_stripe;`.
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
    .map(|(id, key, cents)| price(id, key, cents))
    .collect()
}

/// An active monthly USD per-unit price.
pub fn price(id: &str, lookup_key: &str, unit_amount: i64) -> Value {
    json!({
        "id": id, "object": "price", "active": true, "currency": "usd",
        "lookup_key": lookup_key, "unit_amount": unit_amount, "billing_scheme": "per_unit",
        "type": "recurring", "recurring": { "interval": "month", "interval_count": 1 },
        "livemode": false, "nickname": null,
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

    fn item(&mut self, price_id: &str, quantity: i64, subscription: &str) -> Value {
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
                Some(customer) => ok(customer.clone()),
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
                });
                self.setup_intents.insert(id, intent.clone());
                ok(intent)
            }
            ("GET", ["v1", "setup_intents", id]) => {
                let Some(mut intent) = self.setup_intents.get(*id).cloned() else {
                    return missing();
                };
                if let Some(method) = intent["payment_method"].as_str().map(str::to_string) {
                    intent["payment_method"] = json!({
                        "id": method, "object": "payment_method", "type": "card",
                        "card": { "brand": "visa", "last4": "4242" },
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
                Some(subscription) => ok(subscription.clone()),
                None => missing(),
            },
            ("POST", ["v1", "subscriptions", id]) => {
                let Some(subscription) = self.subscriptions.get_mut(*id) else {
                    return missing();
                };
                for (key, value) in metadata(&params).as_object().into_iter().flatten() {
                    subscription["metadata"][key] = value.clone();
                }
                if let Some(flag) = one("cancel_at_period_end") {
                    subscription["cancel_at_period_end"] = json!(flag == "true");
                }
                if let Some(method) = one("default_payment_method") {
                    subscription["default_payment_method"] = json!(method);
                }
                ok(subscription.clone())
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
