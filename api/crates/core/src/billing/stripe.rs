//! The Stripe REST API (the calls billing makes), webhook signature checks,
//! and Stripe objects read into billing snapshots.
//!
//! Requests pin the API version the CRM's Stripe SDK used, so objects have
//! the same shape (`invoice.parent.subscription_details`, periods on
//! subscription items, `invoice.confirmation_secret`).

use super::catalog::PriceKind;
use super::{BillingEvent, InvoiceSnapshot, ItemSnapshot, SubscriptionSnapshot};
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
    Api(StripeFailure),
    /// The object does not exist (404, e.g. a customer already deleted).
    #[error("{0}")]
    Missing(StripeFailure),
    /// Stripe failed or throttled (5xx, 429): the same request, under the
    /// same idempotency key, may be retried.
    #[error("Stripe request failed: {0}")]
    Server(StripeFailure),
    #[error("Stripe request failed: {0}")]
    Request(String),
}

/// What Stripe said about a refused request. The message can quote what was
/// sent (an email, a phone number): show it to the person who sent it, but
/// log and store [`StripeError::code`] only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeFailure {
    pub message: String,
    /// Stripe's `error.code`, else `error.type`, else `http_<status>`.
    pub code: String,
    /// The HTTP status; 0 when Stripe was not asked.
    pub status: u16,
    /// Stripe's `error.type` (`card_error`, `invalid_request_error`), if any.
    pub kind: Option<String>,
}

impl std::fmt::Display for StripeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl StripeFailure {
    fn new(message: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: code.into(),
            status: 0,
            kind: None,
        }
    }
}

impl StripeError {
    /// Worth retrying later with the same idempotency key.
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Server(_) | Self::Request(_))
    }

    /// A short reason safe to log and store: Stripe's error code or type,
    /// never its message.
    pub fn code(&self) -> &str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Api(failure) | Self::Missing(failure) | Self::Server(failure) => &failure.code,
            Self::Request(_) => "request_failed",
        }
    }
}

/// How long one Stripe request may take: a job starts no work after 20 s and
/// ends inside the API Gateway's 30 s, so one slow call must not outlast it.
/// Stripe POSTs are keyed, so a timed out one is safe to retry.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The Stripe base URL to use and whether `STRIPE_API_BASE` was refused.
#[derive(Debug, PartialEq, Eq)]
pub struct ChosenBase<'a> {
    pub base: &'a str,
    pub ignored: bool,
}

/// `override_base` (`STRIPE_API_BASE`) counts only with a test key, so a
/// variable left over from a local run never sends live traffic elsewhere.
pub fn api_base<'a>(key: Option<&str>, override_base: Option<&'a str>) -> ChosenBase<'a> {
    let override_base = override_base.map(str::trim).filter(|base| !base.is_empty());
    let test_key = key
        .map(str::trim)
        .is_some_and(|key| key.starts_with("sk_test_") || key.starts_with("rk_test_"));
    match override_base {
        Some(base) if test_key => ChosenBase {
            base,
            ignored: false,
        },
        Some(_) => ChosenBase {
            base: API_BASE,
            ignored: true,
        },
        None => ChosenBase {
            base: API_BASE,
            ignored: false,
        },
    }
}

/// A Stripe client; disabled without `STRIPE_SECRET_KEY`.
#[derive(Clone)]
pub struct Stripe {
    key: Option<Arc<str>>,
    base: Arc<str>,
    http: reqwest::Client,
    /// Pause between polls of a subscription that is not paid yet.
    pub poll_delay: Duration,
    /// How long one request may take.
    pub request_timeout: Duration,
}

impl Stripe {
    pub fn disabled() -> Self {
        Self::new(None, API_BASE)
    }

    /// `STRIPE_API_BASE` replaces `https://api.stripe.com` for local e2e runs
    /// against a fake Stripe, and only with a `sk_test_`/`rk_test_` key.
    pub fn from_env() -> Self {
        let key = std::env::var("STRIPE_SECRET_KEY").ok();
        let base = std::env::var("STRIPE_API_BASE").ok();
        let chosen = api_base(key.as_deref(), base.as_deref());
        if chosen.ignored {
            lambda_http::tracing::warn!("STRIPE_API_BASE is ignored: it needs a Stripe test key");
        }
        Self::new(key.as_deref(), chosen.base)
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
            request_timeout: REQUEST_TIMEOUT,
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
        self.send_keyed(method, path, form, None).await
    }

    /// `idempotency_key`: Stripe answers a repeat of the same request with
    /// the first response, so a retry never creates or charges twice.
    async fn send_keyed(
        &self,
        method: reqwest::Method,
        path: &str,
        form: &[(&str, String)],
        idempotency_key: Option<&str>,
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
            .timeout(self.request_timeout);
        let request = match idempotency_key {
            Some(key) => request.header("Idempotency-Key", key),
            None => request,
        };
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
        let body: Value = match response.json().await {
            Ok(body) => body,
            Err(_) if status.is_server_error() => {
                return Err(StripeError::Server(StripeFailure {
                    status: status.as_u16(),
                    ..StripeFailure::new(status.to_string(), format!("http_{}", status.as_u16()))
                }));
            }
            Err(err) => return Err(StripeError::Request(err.without_url().to_string())),
        };
        if status.is_success() {
            return Ok(body);
        }
        let error = &body["error"];
        let code = error["code"]
            .as_str()
            .or_else(|| error["type"].as_str())
            .filter(|code| !code.is_empty())
            .map_or_else(|| format!("http_{}", status.as_u16()), str::to_string);
        let message = StripeFailure {
            status: status.as_u16(),
            kind: error["type"].as_str().map(str::to_string),
            ..StripeFailure::new(
                error["message"].as_str().unwrap_or("Stripe request failed"),
                code,
            )
        };
        Err(
            if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                StripeError::Server(message)
            } else if status == reqwest::StatusCode::NOT_FOUND {
                StripeError::Missing(message)
            } else {
                StripeError::Api(message)
            },
        )
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, StripeError> {
        self.send(reqwest::Method::GET, path, query).await
    }

    async fn post(&self, path: &str, form: &[(&str, String)]) -> Result<Value, StripeError> {
        self.send(reqwest::Method::POST, path, form).await
    }

    async fn post_keyed(
        &self,
        path: &str,
        form: &[(&str, String)],
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.send_keyed(reqwest::Method::POST, path, form, Some(idempotency_key))
            .await
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

    /// [`Self::cancel_subscription`] under an idempotency key.
    pub async fn cancel_subscription_keyed(
        &self,
        id: &str,
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.send_keyed(
            reqwest::Method::DELETE,
            &format!("/v1/subscriptions/{}", path_part(id)),
            &[],
            Some(idempotency_key),
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
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
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
            idempotency_key,
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
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            &format!("/v1/payment_methods/{}/attach", path_part(id)),
            &[("customer", customer.into())],
            idempotency_key,
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

    /// A billing portal page with `configuration`; `payment_method_update`
    /// opens the card form.
    pub async fn create_portal_session(
        &self,
        customer: &str,
        configuration: &str,
        return_url: &str,
        payment_method_update: bool,
    ) -> Result<Value, StripeError> {
        let mut form = vec![
            ("customer", customer.to_string()),
            ("configuration", configuration.to_string()),
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

impl Stripe {
    /// The API base: one Stripe account per base, so per-container caches key on it.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// The account's webhook endpoints, enabled or not (up to 100).
    pub async fn webhook_endpoints(&self) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get("/v1/webhook_endpoints", &[("limit", "100".into())])
            .await?;
        Ok(data(list))
    }

    /// Active billing portal configurations (up to 100).
    pub async fn list_portal_configurations(&self) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get(
                "/v1/billing_portal/configurations",
                &[("active", "true".into()), ("limit", "100".into())],
            )
            .await?;
        Ok(data(list))
    }

    /// A portal configuration tagged `metadata[app] = tag`: cards, invoices and
    /// customer details, but no cancelling or plan changes.
    pub async fn create_portal_configuration(
        &self,
        name: &str,
        tag: &str,
    ) -> Result<Value, StripeError> {
        self.post(
            "/v1/billing_portal/configurations",
            &[
                ("name", name.into()),
                ("metadata[app]", tag.into()),
                ("features[payment_method_update][enabled]", "true".into()),
                ("features[invoice_history][enabled]", "true".into()),
                ("features[customer_update][enabled]", "true".into()),
                (
                    "features[customer_update][allowed_updates][0]",
                    "email".into(),
                ),
                (
                    "features[customer_update][allowed_updates][1]",
                    "address".into(),
                ),
                (
                    "features[customer_update][allowed_updates][2]",
                    "name".into(),
                ),
                ("features[subscription_cancel][enabled]", "false".into()),
                ("features[subscription_update][enabled]", "false".into()),
            ],
        )
        .await
    }

    pub async fn price(&self, id: &str) -> Result<Value, StripeError> {
        self.get(&format!("/v1/prices/{}", path_part(id)), &[])
            .await
    }

    /// The invoice with its payment's confirmation secret for the browser.
    pub async fn invoice_with_confirmation_secret(&self, id: &str) -> Result<Value, StripeError> {
        self.get(
            &format!("/v1/invoices/{}", path_part(id)),
            &[("expand[]", "confirmation_secret".into())],
        )
        .await
    }

    /// The customer with its default card expanded.
    pub async fn customer_with_default_payment_method(
        &self,
        id: &str,
    ) -> Result<Value, StripeError> {
        self.get(
            &format!("/v1/customers/{}", path_part(id)),
            &[("expand[]", "invoice_settings.default_payment_method".into())],
        )
        .await
    }
}

/// The Stripe customer of a self-serve signup.
#[derive(Debug, Clone)]
pub struct SignupCustomer<'a> {
    pub signup_id: &'a str,
    pub company_name: &'a str,
    pub email: &'a str,
    pub phone: &'a str,
    pub line1: &'a str,
    pub city: &'a str,
    pub state: &'a str,
    pub postal_code: &'a str,
}

/// What a self-serve subscription comes from.
///
/// Its idempotency key
/// (`signup:<id>:subscription`, `plan-invite:<id>:subscription`, with
/// `:now` when billed at once) and its origin metadata
/// (`metadata[signup_id]`, `metadata[plan_invite_id]`).
#[derive(Debug, Clone, Copy)]
pub enum SubscriptionOrigin<'a> {
    Signup(&'a str),
    PlanInvite(&'a str),
}

impl SubscriptionOrigin<'_> {
    fn metadata(self) -> (&'static str, String) {
        match self {
            Self::Signup(id) => ("metadata[signup_id]", id.to_string()),
            Self::PlanInvite(id) => ("metadata[plan_invite_id]", id.to_string()),
        }
    }

    /// `bills_now`: `:now` appended, so a retry after the free period
    /// passed never reuses a dated request's key with other parameters.
    fn idempotency_key(self, bills_now: bool) -> String {
        let key = match self {
            Self::Signup(id) => format!("signup:{id}:subscription"),
            Self::PlanInvite(id) => format!("plan-invite:{id}:subscription"),
        };
        if bills_now { format!("{key}:now") } else { key }
    }
}

/// A self-serve subscription (03-stripe.md section 4.4).
#[derive(Debug, Clone)]
pub struct NewSubscription<'a> {
    pub origin: SubscriptionOrigin<'a>,
    pub company_id: i32,
    pub customer: &'a str,
    pub payment_method: &'a str,
    /// Quantity 1 each: the base price first, then the add-ons.
    pub price_ids: &'a [String],
    /// When the free period ends (Unix seconds, UTC); a time not in the
    /// future is sent as `now` (billed at once), under the `:now` key.
    pub trial_end: i64,
    pub automatic_tax: bool,
}

/// Self-serve signup, subscription items and seats. Every POST that creates
/// or changes billing sends an idempotency key built from our ids.
impl Stripe {
    /// Whether the key is a test-mode key (`sk_test_...`).
    pub fn is_test_mode(&self) -> bool {
        self.key
            .as_deref()
            .is_some_and(|key| key.starts_with("sk_test_"))
    }

    /// Active prices by lookup key (up to 10 keys).
    pub async fn prices_by_lookup_keys(&self, keys: &[&str]) -> Result<Vec<Value>, StripeError> {
        let mut query: Vec<(&str, String)> = keys
            .iter()
            .map(|key| ("lookup_keys[]", (*key).to_string()))
            .collect();
        query.push(("active", "true".into()));
        query.push(("limit", "10".into()));
        Ok(data(self.get("/v1/prices", &query).await?))
    }

    /// Idempotency key `signup:<id>:customer`.
    pub async fn create_signup_customer(
        &self,
        customer: &SignupCustomer<'_>,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            "/v1/customers",
            &[
                ("name", customer.company_name.into()),
                ("email", customer.email.into()),
                ("phone", customer.phone.into()),
                ("address[line1]", customer.line1.into()),
                ("address[city]", customer.city.into()),
                ("address[state]", customer.state.into()),
                ("address[postal_code]", customer.postal_code.into()),
                ("address[country]", "US".into()),
                ("metadata[signup_id]", customer.signup_id.into()),
            ],
            &format!("signup:{}:customer", customer.signup_id),
        )
        .await
    }

    /// Links the customer to the new company and, when given, makes
    /// `payment_method` its default. Idempotency key
    /// `signup:<id>:customer-company`, or `signup:<id>:customer-company:awaiting`
    /// without a method (a bank account still waiting for micro-deposits).
    pub async fn link_signup_customer(
        &self,
        customer: &str,
        signup_id: &str,
        company_id: i32,
        payment_method: Option<&str>,
    ) -> Result<Value, StripeError> {
        let mut form = vec![("metadata[company_id]", company_id.to_string())];
        form.extend(
            payment_method
                .map(|method| ("invoice_settings[default_payment_method]", method.to_string())),
        );
        let key = match payment_method {
            Some(_) => format!("signup:{signup_id}:customer-company"),
            None => format!("signup:{signup_id}:customer-company:awaiting"),
        };
        self.post_keyed(
            &format!("/v1/customers/{}", path_part(customer)),
            &form,
            &key,
        )
        .await
    }

    /// Deletes a customer (an abandoned signup's); `Missing` when already gone.
    pub async fn delete_customer(&self, id: &str) -> Result<Value, StripeError> {
        self.send(
            reqwest::Method::DELETE,
            &format!("/v1/customers/{}", path_part(id)),
            &[],
        )
        .await
    }

    /// A SetupIntent for card or bank account, confirmed by the browser.
    /// Idempotency key `signup:<id>:si:<attempt>`.
    pub async fn create_signup_setup_intent(
        &self,
        customer: &str,
        signup_id: &str,
        attempt: u8,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            "/v1/setup_intents",
            &[
                ("customer", customer.into()),
                ("usage", "off_session".into()),
                ("payment_method_types[]", "card".into()),
                ("payment_method_types[]", "us_bank_account".into()),
                (
                    "payment_method_options[us_bank_account][verification_method]",
                    "automatic".into(),
                ),
                (
                    "payment_method_options[us_bank_account][financial_connections][permissions][]",
                    "payment_method".into(),
                ),
                ("metadata[signup_id]", signup_id.into()),
                ("metadata[purpose]", "signup".into()),
            ],
            &format!("signup:{signup_id}:si:{attempt}"),
        )
        .await
    }

    /// The test seam: a card SetupIntent confirmed server-side with a test
    /// payment method (`pm_card_visa`). Only for `sk_test_` keys.
    pub async fn create_confirmed_test_setup_intent(
        &self,
        customer: &str,
        purpose: SetupPurpose<'_>,
        payment_method: &str,
    ) -> Result<Value, StripeError> {
        if !self.is_test_mode() {
            return Err(StripeError::Api(StripeFailure::new(
                "Test payment methods need a test-mode key",
                "test_mode_required",
            )));
        }
        let (origin_key, origin_id, purpose_name, key) = match purpose {
            SetupPurpose::Signup { signup_id, attempt } => (
                "metadata[signup_id]",
                signup_id.to_string(),
                "signup",
                format!("signup:{signup_id}:si:{attempt}"),
            ),
            SetupPurpose::PlanInvite { invite_id } => (
                "metadata[plan_invite_id]",
                invite_id.to_string(),
                "plan_invite",
                format!("plan-invite:{invite_id}:si"),
            ),
            SetupPurpose::Company {
                company_id,
                purpose,
                idempotency_key,
            } => (
                "metadata[company_id]",
                company_id.to_string(),
                purpose,
                idempotency_key.to_string(),
            ),
        };
        self.post_keyed(
            "/v1/setup_intents",
            &[
                ("customer", customer.into()),
                ("usage", "off_session".into()),
                ("payment_method_types[]", "card".into()),
                ("payment_method", payment_method.into()),
                ("confirm", "true".into()),
                (origin_key, origin_id),
                ("metadata[purpose]", purpose_name.into()),
            ],
            &key,
        )
        .await
    }

    pub async fn setup_intent(&self, id: &str) -> Result<Value, StripeError> {
        self.get(
            &format!("/v1/setup_intents/{}", path_part(id)),
            &[("expand[]", "payment_method".into())],
        )
        .await
    }

    /// The self-serve subscription, nothing charged until `trial_end`.
    /// Idempotency key and origin metadata from [`SubscriptionOrigin`].
    pub async fn create_self_serve_subscription(
        &self,
        subscription: &NewSubscription<'_>,
    ) -> Result<Value, StripeError> {
        let (origin_key, origin_id) = subscription.origin.metadata();
        let bills_now = subscription.trial_end <= chrono::Utc::now().timestamp();
        let mut form: Vec<(String, String)> = vec![
            ("customer".into(), subscription.customer.into()),
            (
                "default_payment_method".into(),
                subscription.payment_method.into(),
            ),
            // A first charge day already passed (a bank verified late, D21):
            // Stripe refuses a past time, and `now` keeps the request (and
            // its `:now` idempotency key) the same on a retry.
            (
                "trial_end".into(),
                if bills_now {
                    "now".into()
                } else {
                    subscription.trial_end.to_string()
                },
            ),
            ("billing_mode[type]".into(), "flexible".into()),
            (
                "trial_settings[end_behavior][missing_payment_method]".into(),
                "create_invoice".into(),
            ),
            (
                "payment_settings[payment_method_types][]".into(),
                "card".into(),
            ),
            (
                "payment_settings[payment_method_types][]".into(),
                "us_bank_account".into(),
            ),
            ("collection_method".into(), "charge_automatically".into()),
            (
                "metadata[company_id]".into(),
                subscription.company_id.to_string(),
            ),
            (origin_key.into(), origin_id),
            ("metadata[plan]".into(), SELF_SERVE_PLAN.into()),
        ];
        for (index, price) in subscription.price_ids.iter().enumerate() {
            form.push((format!("items[{index}][price]"), price.clone()));
            form.push((format!("items[{index}][quantity]"), "1".into()));
        }
        if subscription.automatic_tax {
            form.push(("automatic_tax[enabled]".into(), "true".into()));
        }
        let form: Vec<(&str, String)> = form
            .iter()
            .map(|(name, value)| (name.as_str(), value.clone()))
            .collect();
        self.post_keyed(
            "/v1/subscriptions",
            &form,
            &subscription.origin.idempotency_key(bills_now),
        )
        .await
    }

    /// Adds a priced item to the subscription (prorated).
    pub async fn create_subscription_item(
        &self,
        subscription: &str,
        price_id: &str,
        quantity: i64,
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            "/v1/subscription_items",
            &[
                ("subscription", subscription.into()),
                ("price", price_id.into()),
                ("quantity", quantity.to_string()),
                ("proration_behavior", "create_prorations".into()),
            ],
            idempotency_key,
        )
        .await
    }

    /// Sets an item's quantity (prorated); never sends `price`, which would
    /// reset the quantity.
    pub async fn update_subscription_item_quantity(
        &self,
        item_id: &str,
        quantity: i64,
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            &format!("/v1/subscription_items/{}", path_part(item_id)),
            &[
                ("quantity", quantity.to_string()),
                ("proration_behavior", "create_prorations".into()),
            ],
            idempotency_key,
        )
        .await
    }

    /// The next invoice of the subscription as Stripe would draft it now.
    /// Read only: the key is unique per call, so a stale preview is never
    /// replayed.
    pub async fn preview_invoice(
        &self,
        customer: &str,
        subscription: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            "/v1/invoices/create_preview",
            &[
                ("customer", customer.into()),
                ("subscription", subscription.into()),
            ],
            &format!("preview:{subscription}:{}", uuid::Uuid::new_v4()),
        )
        .await
    }

    /// Schedules the subscription to end at its period end (`true`), or
    /// withdraws that (`false`). `comment`: the customer's reason.
    pub async fn set_cancel_at_period_end(
        &self,
        subscription: &str,
        cancel: bool,
        comment: Option<&str>,
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        let mut form = vec![("cancel_at_period_end", cancel.to_string())];
        if let Some(comment) = comment {
            form.push(("cancellation_details[comment]", comment.into()));
        }
        self.post_keyed(
            &format!("/v1/subscriptions/{}", path_part(subscription)),
            &form,
            idempotency_key,
        )
        .await
    }

    /// Removes an item (prorated); `Missing` when already gone.
    pub async fn delete_subscription_item(&self, item_id: &str) -> Result<Value, StripeError> {
        self.send(
            reqwest::Method::DELETE,
            &format!("/v1/subscription_items/{}", path_part(item_id)),
            &[("proration_behavior", "create_prorations".into())],
        )
        .await
    }
}

/// `metadata[plan]` of self-serve subscriptions.
pub const SELF_SERVE_PLAN: &str = "self_serve_v1";

/// Card and bank account payment methods (step 13). Every POST carries an
/// idempotency key; none sends a `statement_descriptor` (the Dashboard's
/// applies, D14).
impl Stripe {
    /// A SetupIntent for a signed-in Manager's card or bank account, confirmed
    /// by the browser: `metadata[company_id]`, `metadata[purpose]`
    /// (`payment_method_update`, `pay` or `plan_invite`) and, for an invite,
    /// `metadata[plan_invite_id]`. No `link` (D3), verification `automatic`
    /// (instant, else micro-deposits).
    pub async fn create_company_setup_intent(
        &self,
        customer: &str,
        company_id: i32,
        purpose: &str,
        plan_invite_id: Option<&str>,
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        let mut form: Vec<(&str, String)> = vec![
            ("customer", customer.into()),
            ("usage", "off_session".into()),
            ("payment_method_types[]", "card".into()),
            ("payment_method_types[]", "us_bank_account".into()),
            (
                "payment_method_options[us_bank_account][verification_method]",
                "automatic".into(),
            ),
            (
                "payment_method_options[us_bank_account][financial_connections][permissions][]",
                "payment_method".into(),
            ),
            ("metadata[company_id]", company_id.to_string()),
            ("metadata[purpose]", purpose.into()),
        ];
        if let Some(invite) = plan_invite_id {
            form.push(("metadata[plan_invite_id]", invite.into()));
        }
        self.post_keyed("/v1/setup_intents", &form, idempotency_key)
            .await
    }

    /// Cancels a SetupIntent nobody will finish (a bank account replaced
    /// before it was verified); `Missing` when it is gone.
    pub async fn cancel_setup_intent(
        &self,
        id: &str,
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            &format!("/v1/setup_intents/{}/cancel", path_part(id)),
            &[],
            idempotency_key,
        )
        .await
    }

    /// Detaches a payment method from its customer (a replaced one);
    /// `Missing` when it is gone.
    pub async fn detach_payment_method(
        &self,
        id: &str,
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            &format!("/v1/payment_methods/{}/detach", path_part(id)),
            &[],
            idempotency_key,
        )
        .await
    }

    /// Makes `payment_method` the customer's default for invoices.
    pub async fn set_default_payment_method_keyed(
        &self,
        customer: &str,
        payment_method: &str,
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            &format!("/v1/customers/{}", path_part(customer)),
            &[(
                "invoice_settings[default_payment_method]",
                payment_method.into(),
            )],
            idempotency_key,
        )
        .await
    }

    /// Charges the subscription to `payment_method` from now on.
    pub async fn set_subscription_payment_method_keyed(
        &self,
        id: &str,
        payment_method: &str,
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            &format!("/v1/subscriptions/{}", path_part(id)),
            &[("default_payment_method", payment_method.into())],
            idempotency_key,
        )
        .await
    }

    /// Charges an open invoice now, with `payment_method` when given (else
    /// the invoice's default). A card decline is `StripeError::Api` with
    /// kind `card_error`; a card that needs 3D Secure is the code
    /// `invoice_payment_intent_requires_action`; a bank debit answers the
    /// invoice still `open` while the payment processes.
    pub async fn pay_invoice_with(
        &self,
        id: &str,
        payment_method: Option<&str>,
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        let form: Vec<(&str, String)> = payment_method
            .map(|method| vec![("payment_method", method.to_string())])
            .unwrap_or_default();
        self.post_keyed(
            &format!("/v1/invoices/{}/pay", path_part(id)),
            &form,
            idempotency_key,
        )
        .await
    }

    pub async fn charge(&self, id: &str) -> Result<Value, StripeError> {
        self.get(&format!("/v1/charges/{}", path_part(id)), &[])
            .await
    }

    pub async fn payment_intent(&self, id: &str) -> Result<Value, StripeError> {
        self.get(&format!("/v1/payment_intents/{}", path_part(id)), &[])
            .await
    }
}

/// What a test-seam SetupIntent is for: its idempotency key and metadata.
#[derive(Debug, Clone, Copy)]
pub enum SetupPurpose<'a> {
    /// `signup:<id>:si:<attempt>`, `metadata[signup_id]`, purpose `signup`.
    Signup { signup_id: &'a str, attempt: u8 },
    /// `plan-invite:<id>:si`, `metadata[plan_invite_id]`, purpose `plan_invite`.
    PlanInvite { invite_id: &'a str },
    /// A signed-in Manager's update (`payment_method_update`) or pay page
    /// (`pay`): `metadata[company_id]`, the caller's idempotency key
    /// (`billing-setup:<company_id>:<uuid>`).
    Company {
        company_id: i32,
        purpose: &'a str,
        idempotency_key: &'a str,
    },
}

/// The Stripe customer of a plan invite's company (when it has none yet).
#[derive(Debug, Clone)]
pub struct InviteCustomer<'a> {
    pub invite_id: &'a str,
    pub company_id: i32,
    pub company_name: &'a str,
    /// The company's first Manager's email (the same for every Manager who
    /// accepts), or none.
    pub email: Option<&'a str>,
}

/// Moving a legacy company onto a plan (plan invites). Every POST carries
/// an idempotency key built from the invite id.
impl Stripe {
    /// Idempotency key `plan-invite:<id>:customer`.
    pub async fn create_invite_customer(
        &self,
        customer: &InviteCustomer<'_>,
    ) -> Result<Value, StripeError> {
        let mut form = vec![("name", customer.company_name.to_string())];
        if let Some(email) = customer.email {
            form.push(("email", email.to_string()));
        }
        form.push(("metadata[company_id]", customer.company_id.to_string()));
        form.push(("metadata[plan_invite_id]", customer.invite_id.to_string()));
        self.post_keyed(
            "/v1/customers",
            &form,
            &format!("plan-invite:{}:customer", customer.invite_id),
        )
        .await
    }

    /// Makes `payment_method` the customer's default. Idempotency key
    /// `plan-invite:<id>:customer-pm`.
    pub async fn set_invite_default_payment_method(
        &self,
        customer: &str,
        payment_method: &str,
        invite_id: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            &format!("/v1/customers/{}", path_part(customer)),
            &[(
                "invoice_settings[default_payment_method]",
                payment_method.into(),
            )],
            &format!("plan-invite:{invite_id}:customer-pm"),
        )
        .await
    }

    /// The legacy subscription ends at its period end, marked as replaced
    /// by `replaced_by` (billing then never follows it again). Idempotency
    /// key `plan-invite:<id>:replace-legacy`.
    pub async fn replace_legacy_subscription(
        &self,
        legacy_id: &str,
        replaced_by: &str,
        invite_id: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            &format!("/v1/subscriptions/{}", path_part(legacy_id)),
            &[
                ("cancel_at_period_end", "true".into()),
                ("metadata[replaced_by]", replaced_by.into()),
                ("metadata[plan_invite_id]", invite_id.into()),
            ],
            &format!("plan-invite:{invite_id}:replace-legacy"),
        )
        .await
    }

    /// Undoes [`Self::replace_legacy_subscription`]: the legacy subscription
    /// renews again, its `replaced_by` and `plan_invite_id` cleared.
    /// Idempotency key `plan-invite:<id>:restore-legacy`.
    pub async fn restore_legacy_subscription(
        &self,
        legacy_id: &str,
        invite_id: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            &format!("/v1/subscriptions/{}", path_part(legacy_id)),
            &[
                ("cancel_at_period_end", "false".into()),
                ("metadata[replaced_by]", String::new()),
                ("metadata[plan_invite_id]", String::new()),
            ],
            &format!("plan-invite:{invite_id}:restore-legacy"),
        )
        .await
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
    let items: Vec<ItemSnapshot> = subscription["items"]["data"]
        .as_array()
        .map(|items| items.iter().map(item_snapshot).collect())
        .unwrap_or_default();
    let multi_item = items.iter().any(|item| {
        item.lookup_key
            .as_deref()
            .is_some_and(|key| PriceKind::from_lookup_key(key).is_some())
    });
    let first = &subscription["items"]["data"][0];
    let item = if multi_item {
        subscription["items"]["data"]
            .as_array()
            .and_then(|raw| {
                raw.iter().find(|item| {
                    item["price"]["lookup_key"].as_str() == Some(super::catalog::BASE_LOOKUP_KEY)
                })
            })
            .unwrap_or(first)
    } else {
        first
    };
    let price = &item["price"];
    let monthly_total_cents = multi_item.then(|| {
        items
            .iter()
            .filter_map(|item| Some(item.unit_amount_cents? * item.quantity))
            .sum()
    });
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
        items,
        monthly_total_cents,
        trial_end: unix_date(&subscription["trial_end"]),
        cancel_at: unix_date(&subscription["cancel_at"]).or_else(|| {
            (subscription["cancel_at_period_end"].as_bool() == Some(true))
                .then(|| unix_date(&item["current_period_end"]))
                .flatten()
        }),
        replaced_by: non_empty(&subscription["metadata"]["replaced_by"]),
        plan_invite_id: non_empty(&subscription["metadata"]["plan_invite_id"]),
    }
}

fn item_snapshot(item: &Value) -> ItemSnapshot {
    let price = &item["price"];
    ItemSnapshot {
        id: text(&item["id"]).unwrap_or_default(),
        price_id: text(&price["id"]).unwrap_or_default(),
        lookup_key: non_empty(&price["lookup_key"]),
        quantity: item["quantity"].as_i64().unwrap_or(1),
        unit_amount_cents: price["unit_amount"].as_i64(),
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
        live_subscription_status: None,
        replaced_by: non_empty(&details["metadata"]["replaced_by"]),
        plan_invite_id: non_empty(&details["metadata"]["plan_invite_id"]),
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
        (kind @ ("invoice.voided" | "invoice.marked_uncollectible"), "invoice") => {
            Some(BillingEvent::InvoiceClosed {
                id,
                kind: if kind == "invoice.voided" {
                    "invoice.voided"
                } else {
                    "invoice.marked_uncollectible"
                },
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
/// a voided one is dropped (`None`: its `invoice.voided` event applies it). A closed (voided, uncollectible)
/// invoice that has since been paid becomes `invoice.paid`. A paid or closed invoice carries its subscription's
/// live status, so it restores access only once no other invoice of it is unpaid, and never for a subscription
/// that has ended. Without a client the event is used as sent.
pub async fn latest_event(
    stripe: &Stripe,
    event: BillingEvent,
) -> Result<Option<BillingEvent>, StripeError> {
    if !stripe.is_enabled() {
        return Ok(Some(event));
    }
    let event = match event {
        BillingEvent::Subscription {
            id,
            kind,
            subscription,
        } if kind != "customer.subscription.deleted" => {
            let live = stripe.subscription(&subscription.id).await?;
            return Ok(Some(BillingEvent::Subscription {
                id,
                kind,
                subscription: subscription_snapshot(&live),
            }));
        }
        BillingEvent::Invoice {
            id,
            paid: false,
            invoice,
        } => {
            let live = stripe.invoice(&invoice.id).await?;
            match live["status"].as_str() {
                Some("void") => return Ok(None),
                status => BillingEvent::Invoice {
                    id,
                    paid: status == Some("paid"),
                    invoice: invoice_snapshot(&live),
                },
            }
        }
        BillingEvent::InvoiceClosed { id, kind, invoice } => {
            let live = stripe.invoice(&invoice.id).await?;
            match live["status"].as_str() {
                Some("paid") => BillingEvent::Invoice {
                    id,
                    paid: true,
                    invoice: invoice_snapshot(&live),
                },
                Some("void" | "uncollectible") => BillingEvent::InvoiceClosed {
                    id,
                    kind,
                    invoice: invoice_snapshot(&live),
                },
                // Not closed after all (a draft or open invoice): nothing to apply.
                _ => return Ok(None),
            }
        }
        other => other,
    };
    Ok(Some(match event {
        BillingEvent::Invoice {
            id,
            paid: true,
            invoice,
        } if invoice.subscription_id.is_some() => BillingEvent::Invoice {
            id,
            paid: true,
            invoice: with_live_subscription(stripe, invoice).await?,
        },
        BillingEvent::InvoiceClosed { id, kind, invoice } if invoice.subscription_id.is_some() => {
            BillingEvent::InvoiceClosed {
                id,
                kind,
                invoice: with_live_subscription(stripe, invoice).await?,
            }
        }
        other => other,
    }))
}

/// The invoice with its subscription's live status and plan-move metadata.
async fn with_live_subscription(
    stripe: &Stripe,
    mut invoice: InvoiceSnapshot,
) -> Result<InvoiceSnapshot, StripeError> {
    let subscription_id = invoice.subscription_id.as_deref().unwrap_or_default();
    let live = match stripe.subscription(subscription_id).await {
        Ok(live) => live,
        // Gone from Stripe: applied as sent.
        Err(StripeError::Missing(_)) => return Ok(invoice),
        Err(err) => return Err(err),
    };
    invoice.live_subscription_status = live["status"].as_str().map(str::to_string);
    invoice.replaced_by = invoice
        .replaced_by
        .or_else(|| non_empty(&live["metadata"]["replaced_by"]));
    invoice.plan_invite_id = invoice
        .plan_invite_id
        .or_else(|| non_empty(&live["metadata"]["plan_invite_id"]));
    Ok(invoice)
}

/// `metadata[kind]` of the onboarding advance's invoice item (and of its
/// invoice line).
pub const ADVANCE_ITEM_KIND: &str = "onboarding_advance";
/// `metadata[kind]` of the credit that gives the advance back.
pub const ADVANCE_CREDIT_KIND: &str = "onboarding_advance_credit";

/// The onboarding advance: a month of the subscription paid in advance.
#[derive(Debug, Clone)]
pub struct AdvanceItem<'a> {
    pub customer: &'a str,
    /// The subscription whose next invoice sweeps the item in.
    pub subscription: &'a str,
    pub company_id: i32,
    /// `YYYY-MM-DD` of the deadline it is for.
    pub deadline: &'a str,
    pub amount_cents: i64,
    /// `Advance payment for <month range>`.
    pub description: &'a str,
    /// `onboarding-advance:<company_id>:<deadline>[:<n>]`.
    pub idempotency_key: &'a str,
}

/// The onboarding advance (Terms 11): a pending invoice item on the
/// subscription, swept by Stripe into its next invoice, and a customer
/// balance credit once that invoice is paid. Every POST carries an
/// idempotency key.
impl Stripe {
    /// A pending USD invoice item for the subscription's next invoice,
    /// tagged `metadata[kind]=onboarding_advance`, `company_id`, `deadline`.
    pub async fn create_advance_item(&self, item: &AdvanceItem<'_>) -> Result<Value, StripeError> {
        self.post_keyed(
            "/v1/invoiceitems",
            &[
                ("customer", item.customer.into()),
                ("subscription", item.subscription.into()),
                ("amount", item.amount_cents.to_string()),
                ("currency", "usd".into()),
                ("description", item.description.into()),
                ("metadata[kind]", ADVANCE_ITEM_KIND.into()),
                ("metadata[company_id]", item.company_id.to_string()),
                ("metadata[deadline]", item.deadline.into()),
            ],
            item.idempotency_key,
        )
        .await
    }

    /// An invoice item; its `invoice` is null while pending. `Missing` when
    /// deleted.
    pub async fn invoice_item(&self, id: &str) -> Result<Value, StripeError> {
        self.get(&format!("/v1/invoiceitems/{}", path_part(id)), &[])
            .await
    }

    /// The customer's invoice items, newest first (up to 100), pending or not.
    pub async fn customer_invoice_items(&self, customer: &str) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get(
                "/v1/invoiceitems",
                &[("customer", customer.into()), ("limit", "100".into())],
            )
            .await?;
        Ok(data(list))
    }

    /// Deletes a pending invoice item (or one on a draft invoice); `Missing`
    /// when already gone.
    pub async fn delete_invoice_item(&self, id: &str) -> Result<Value, StripeError> {
        self.send(
            reqwest::Method::DELETE,
            &format!("/v1/invoiceitems/{}", path_part(id)),
            &[],
        )
        .await
    }

    /// The customer's balance transactions, newest first (up to 100).
    pub async fn balance_transactions(&self, customer: &str) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get(
                &format!("/v1/customers/{}/balance_transactions", path_part(customer)),
                &[("limit", "100".into())],
            )
            .await?;
        Ok(data(list))
    }

    /// Credits `amount_cents` (USD) to the customer's balance: Stripe applies
    /// it to the next invoice. Tagged `metadata[kind]=onboarding_advance_credit`,
    /// `company_id`, `invoice`.
    pub async fn credit_customer_balance(
        &self,
        customer: &str,
        company_id: i32,
        invoice_id: &str,
        amount_cents: i64,
        description: &str,
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            &format!("/v1/customers/{}/balance_transactions", path_part(customer)),
            &[
                ("amount", (-amount_cents).to_string()),
                ("currency", "usd".into()),
                ("description", description.into()),
                ("metadata[kind]", ADVANCE_CREDIT_KIND.into()),
                ("metadata[company_id]", company_id.to_string()),
                ("metadata[invoice]", invoice_id.into()),
            ],
            idempotency_key,
        )
        .await
    }

    /// Ends the subscription at `cancel_at` (a period boundary) without
    /// proration, or (`None`) removes a set end. `comment`: the customer's
    /// reason.
    pub async fn set_cancel_at(
        &self,
        subscription: &str,
        cancel_at: Option<NaiveDateTime>,
        comment: Option<&str>,
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        let mut form = vec![(
            "cancel_at",
            cancel_at.map_or_else(String::new, |at| at.and_utc().timestamp().to_string()),
        )];
        if cancel_at.is_some() {
            form.push(("proration_behavior", "none".into()));
        }
        if let Some(comment) = comment {
            form.push(("cancellation_details[comment]", comment.into()));
        }
        self.post_keyed(
            &format!("/v1/subscriptions/{}", path_part(subscription)),
            &form,
            idempotency_key,
        )
        .await
    }
}

/// Account setup (`billing::stripe_setup`, `examples/stripe_setup.rs`). Every
/// write takes the caller's idempotency key.
impl Stripe {
    /// `GET /v1/balance`, read only for its `livemode`.
    pub async fn balance(&self) -> Result<Value, StripeError> {
        self.get("/v1/balance", &[]).await
    }

    /// The account's payment method configurations (up to 100); the default
    /// one has `is_default = true`.
    pub async fn payment_method_configurations(&self) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get(
                "/v1/payment_method_configurations",
                &[("limit", "100".into())],
            )
            .await?;
        Ok(data(list))
    }

    pub async fn create_price_keyed(
        &self,
        form: &[(&str, String)],
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed("/v1/prices", form, idempotency_key).await
    }

    pub async fn create_portal_configuration_keyed(
        &self,
        form: &[(&str, String)],
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed("/v1/billing_portal/configurations", form, idempotency_key)
            .await
    }

    pub async fn update_portal_configuration(
        &self,
        id: &str,
        form: &[(&str, String)],
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            &format!("/v1/billing_portal/configurations/{}", path_part(id)),
            form,
            idempotency_key,
        )
        .await
    }

    /// The answer carries the signing secret (`secret`) once, on creation.
    pub async fn create_webhook_endpoint(
        &self,
        form: &[(&str, String)],
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed("/v1/webhook_endpoints", form, idempotency_key)
            .await
    }

    pub async fn update_webhook_endpoint(
        &self,
        id: &str,
        form: &[(&str, String)],
        idempotency_key: &str,
    ) -> Result<Value, StripeError> {
        self.post_keyed(
            &format!("/v1/webhook_endpoints/{}", path_part(id)),
            form,
            idempotency_key,
        )
        .await
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
    fn subscriptions_read_their_trial_end_and_scheduled_end() {
        let base = |extra: Value| {
            let mut subscription = json!({
                "id": "sub_1", "status": "trialing", "trial_end": 1_800_000_000,
                "items": { "data": [{ "current_period_end": 1_800_000_000 }] },
            });
            if let (Some(target), Some(extra)) = (subscription.as_object_mut(), extra.as_object()) {
                target.extend(extra.clone());
            }
            subscription_snapshot(&subscription)
        };
        let date = DateTime::from_timestamp(1_800_000_000, 0)
            .unwrap()
            .naive_utc();
        assert_eq!(base(json!({})).trial_end, Some(date));
        assert_eq!(base(json!({})).cancel_at, None);
        assert_eq!(
            base(json!({"cancel_at_period_end": true})).cancel_at,
            Some(date)
        );
        assert_eq!(
            base(json!({"cancel_at": 1_700_000_000}))
                .cancel_at
                .map(|at| at < date),
            Some(true)
        );
        assert_eq!(base(json!({"trial_end": null})).trial_end, None);
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
    fn voided_and_uncollectible_invoices_are_closed_invoice_events() {
        for kind in ["invoice.voided", "invoice.marked_uncollectible"] {
            let event = normalize_event(&json!({ "id": "evt", "type": kind,
                "data": { "object": { "object": "invoice", "id": "in_1", "customer": "cus_1",
                    "status": "void", "amount_due": 4900,
                    "parent": { "subscription_details": { "subscription": "sub_1" } } } } }));
            let Some(BillingEvent::InvoiceClosed {
                kind: read,
                invoice,
                ..
            }) = event
            else {
                panic!("{kind} not normalized");
            };
            assert_eq!(read, kind);
            assert_eq!(invoice.subscription_id.as_deref(), Some("sub_1"));
        }
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

    #[test]
    fn a_base_override_is_honoured_only_with_a_test_key() {
        let fake = Some("http://127.0.0.1:12111");
        for key in ["sk_test_a", "rk_test_a", " sk_test_a "] {
            let chosen = api_base(Some(key), fake);
            assert_eq!((chosen.base, chosen.ignored), ("http://127.0.0.1:12111", false));
        }
        for key in [Some("sk_live_a"), Some("rk_live_a"), Some("junk"), None] {
            let chosen = api_base(key, fake);
            assert_eq!((chosen.base, chosen.ignored), (API_BASE, true));
        }
        let none = api_base(Some("sk_live_a"), Some("  "));
        assert_eq!((none.base, none.ignored), (API_BASE, false));
    }

    #[test]
    fn requests_time_out_well_inside_the_gateway() {
        assert!(Stripe::disabled().request_timeout <= Duration::from_secs(10));
    }
}
