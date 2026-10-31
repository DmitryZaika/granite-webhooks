//! The payments page (`payments.granite-manager.com`, `apps/payments` in the
//! CRM repo) and the superuser screen that follows the payments.
//!
//! - `GET  /v1/public/payments/{token}`          → the company, its price and its subscription
//! - `POST /v1/public/payments/{token}/checkout` → a Stripe Checkout link that starts the subscription
//! - `POST /v1/public/payments/{token}/portal`   → a Stripe billing portal link (cancel, card, invoices)
//! - `GET  /v1/superuser/payments`               → every company: price, link, last payment, status
//! - `PUT  /v1/superuser/companies/{id}/subscription-price` → a company's monthly price
//!
//! A company's link carries `<company id>.<signature>` (HMAC-SHA256 with
//! `PAYMENTS_LINK_SECRET`), so links cannot be guessed or changed to another
//! company. Stripe keeps the subscriptions and payments and is read on every
//! request; the database keeps only `company.stripe_customer_id` (the link
//! between the two, saved before the first checkout) and
//! `company.subscription_price_cents` (read when a subscription starts).
//! Nothing here changes access to the CRM.

use crate::auth::CurrentUser;
use crate::billing::stripe::{Stripe, StripeError};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::sync::Arc;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(payment_page))
        .routes(routes!(start_checkout))
        .routes(routes!(open_portal))
        .routes(routes!(list_payments))
        .routes(routes!(set_subscription_price))
}

/// Where the payments page is when `PAYMENTS_APP_URL` is not set.
pub const DEFAULT_APP_URL: &str = "https://payments.granite-manager.com";
/// The Stripe product every subscription is sold under (created on first use).
pub const PRODUCT_ID: &str = "granite_manager_subscription";
const PRODUCT_NAME: &str = "Granite Manager subscription";
/// Stripe's tax code for "Software as a service (SaaS) - business use".
pub const PRODUCT_TAX_CODE: &str = "txcd_10103001";
/// Bytes of the HMAC kept in a link: 128 bits, 22 characters.
const SIGNATURE_BYTES: usize = 16;
/// Stripe's smallest USD charge is $0.50; the largest price accepted, $100,000.
const MIN_PRICE_CENTS: i32 = 50;
const MAX_PRICE_CENTS: i32 = 10_000_000;
/// Companies whose Stripe data is read at the same time.
const STRIPE_CONCURRENCY: usize = 4;
/// Subscriptions that still belong to the company: a new one must not be
/// started next to them, and the page sends their owner to the portal.
const LIVE_STATUSES: [&str; 6] = [
    "active",
    "trialing",
    "past_due",
    "unpaid",
    "paused",
    "incomplete",
];

/// The payment links' secret and the payments page's address.
#[derive(Clone)]
pub struct Payments {
    secret: Option<Arc<[u8]>>,
    app_url: Arc<str>,
}

impl Payments {
    /// `secret` empty: links are off (the routes answer 503).
    pub fn new(secret: &str, app_url: &str) -> Self {
        let secret = secret.trim();
        let app_url = app_url.trim().trim_end_matches('/');
        Self {
            secret: (!secret.is_empty()).then(|| Arc::from(secret.as_bytes())),
            app_url: Arc::from(if app_url.is_empty() {
                DEFAULT_APP_URL
            } else {
                app_url
            }),
        }
    }

    /// `PAYMENTS_LINK_SECRET` and `PAYMENTS_APP_URL`.
    pub fn from_env() -> Self {
        Self::new(
            &std::env::var("PAYMENTS_LINK_SECRET").unwrap_or_default(),
            &std::env::var("PAYMENTS_APP_URL").unwrap_or_default(),
        )
    }

    /// The settings `state` was given (`AppState::with_extension`), else the
    /// environment's.
    pub fn of(state: &AppState) -> Arc<Self> {
        state.extensions.get_or_insert_with(Self::from_env)
    }

    fn mac(&self, company_id: i32) -> Option<Hmac<Sha256>> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_deref()?).ok()?;
        mac.update(format!("granite-payments-link:{company_id}").as_bytes());
        Some(mac)
    }

    /// `<company id>.<signature>`, the part of a link that names the company.
    pub fn token(&self, company_id: i32) -> Option<String> {
        let tag = self.mac(company_id)?.finalize().into_bytes();
        Some(format!(
            "{company_id}.{}",
            URL_SAFE_NO_PAD.encode(&tag[..SIGNATURE_BYTES])
        ))
    }

    /// The company a token was made for; `None` when it was not made with
    /// this secret.
    pub fn company_of(&self, token: &str) -> Option<i32> {
        let (id, signature) = token.trim().split_once('.')?;
        let company_id = id.parse::<i32>().ok().filter(|id| *id > 0)?;
        if id != company_id.to_string() {
            return None;
        }
        let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
        if signature.len() != SIGNATURE_BYTES {
            return None;
        }
        self.mac(company_id)?
            .verify_truncated_left(&signature)
            .ok()
            .map(|()| company_id)
    }

    /// The page's address for the company: `<app>/?c=<token>`.
    pub fn link(&self, company_id: i32) -> Option<String> {
        self.token(company_id)
            .map(|token| format!("{}/?c={token}", self.app_url))
    }

    pub fn app_url(&self) -> &str {
        &self.app_url
    }

    pub const fn is_enabled(&self) -> bool {
        self.secret.is_some()
    }
}

fn stripe_error(error: StripeError) -> ApiError {
    match error {
        StripeError::NotConfigured => ApiError::Unavailable("Payments are not set up".into()),
        StripeError::Api(message) => ApiError::Upstream(format!("Stripe: {message}")),
        StripeError::Request(message) => {
            ApiError::Upstream(format!("Stripe could not be reached: {message}"))
        }
    }
}

/// Stripe and the links, or 503 when either is not configured.
fn configured(state: &AppState) -> ApiResult<Arc<Payments>> {
    let payments = Payments::of(state);
    if payments.is_enabled() && state.stripe.is_enabled() {
        Ok(payments)
    } else {
        Err(ApiError::Unavailable("Payments are not set up".into()))
    }
}

#[derive(sqlx::FromRow)]
struct CompanyRow {
    id: i32,
    name: String,
    subscription_price_cents: i32,
    stripe_customer_id: Option<String>,
}

/// The company a link names; 404 for a forged link or a deleted company.
async fn linked_company(
    state: &AppState,
    payments: &Payments,
    token: &str,
) -> ApiResult<CompanyRow> {
    let company_id = payments.company_of(token).ok_or(ApiError::NotFound)?;
    sqlx::query_as(
        "SELECT id, COALESCE(NULLIF(TRIM(name), ''), CONCAT('Company ', id)) AS name,
                subscription_price_cents, NULLIF(stripe_customer_id, '') AS stripe_customer_id
           FROM company WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound)
}

/// A Stripe subscription as the page and the superuser screen show it.
#[derive(Debug, Serialize, ToSchema, PartialEq, Eq)]
pub struct SubscriptionSummary {
    /// Stripe's subscription id (`sub_...`).
    pub id: String,
    /// Stripe's status: `active`, `trialing`, `past_due`, `unpaid`, `paused`,
    /// `incomplete` (first payment pending), `incomplete_expired` or `canceled`.
    pub status: String,
    /// What each month costs, in US cents; null when Stripe has no amount.
    pub price_cents: Option<i64>,
    /// True when Stripe will charge it again: a live subscription that is not
    /// set to cancel.
    pub renews: bool,
    /// The next charge, when it renews (the end of the current period).
    pub next_payment_at: Option<DateTime<Utc>>,
    /// When a subscription set to cancel stops (no charge after it); null
    /// otherwise.
    pub ends_at: Option<DateTime<Utc>>,
}

fn unix(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(value.as_i64()?, 0)
}

impl SubscriptionSummary {
    fn from_stripe(subscription: &Value) -> Self {
        let item = &subscription["items"]["data"][0];
        let status = subscription["status"].as_str().unwrap_or_default();
        let live = matches!(status, "active" | "trialing" | "past_due");
        // Since API 2025-03-31 the period is on the item; older objects have
        // it on the subscription.
        let period_end =
            unix(&item["current_period_end"]).or_else(|| unix(&subscription["current_period_end"]));
        let cancels = subscription["cancel_at_period_end"].as_bool() == Some(true)
            || !subscription["cancel_at"].is_null();
        let ends_at = if cancels && status != "canceled" {
            unix(&subscription["cancel_at"]).or(period_end)
        } else {
            None
        };
        let renews = live && !cancels;
        let price = &item["price"];
        let quantity = item["quantity"].as_i64().unwrap_or(1);
        Self {
            id: subscription["id"].as_str().unwrap_or_default().to_string(),
            status: status.to_string(),
            price_cents: price["unit_amount"]
                .as_i64()
                .map(|amount| amount * quantity),
            renews,
            next_payment_at: if renews { period_end } else { None },
            ends_at,
        }
    }
}

/// The subscription that still belongs to the customer (newest first), if any.
fn live_subscription(subscriptions: &[Value]) -> Option<&Value> {
    subscriptions.iter().find(|subscription| {
        subscription["status"]
            .as_str()
            .is_some_and(|status| LIVE_STATUSES.contains(&status))
    })
}

async fn live_summary(
    stripe: &Stripe,
    customer: Option<&str>,
) -> ApiResult<Option<SubscriptionSummary>> {
    let Some(customer) = customer else {
        return Ok(None);
    };
    let subscriptions = stripe
        .list_subscriptions(customer)
        .await
        .map_err(stripe_error)?;
    Ok(live_subscription(&subscriptions).map(SubscriptionSummary::from_stripe))
}

/// What the payments page shows for a company.
#[derive(Debug, Serialize, ToSchema)]
pub struct PaymentPage {
    /// The company's name.
    pub company_name: String,
    /// The monthly price a new subscription starts at, in US cents.
    pub price_cents: i32,
    /// The company's live subscription (active, trialing, past due, unpaid,
    /// paused or incomplete, possibly set to cancel); null when it has none
    /// and can subscribe.
    pub subscription: Option<SubscriptionSummary>,
}

/// Show a company's payment page
///
/// The company named by the payment link, the monthly price a new
/// subscription would charge, and its live Stripe subscription if it has one.
/// No session: the signed link is the credential. Next: `POST
/// /v1/public/payments/{token}/checkout` to subscribe, or `POST
/// /v1/public/payments/{token}/portal` to manage a subscription.
#[crate::openapi::path(
    get,
    path = "/v1/public/payments/{token}",
    operation_id = "get_payment_page",
    tag = "payments",
    params(("token" = String, Path, description = "The `c` value of the company's payment link: `<company id>.<signature>`")),
    responses(
        (status = 200, description = "The company and its subscription", body = PaymentPage),
        (status = 404, description = "The link is not valid, or the company was deleted", body = ErrorBody),
        (status = 502, description = "Stripe failed", body = ErrorBody),
        (status = 503, description = "Payments are not set up", body = ErrorBody),
    ),
)]
pub async fn payment_page(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<Json<PaymentPage>> {
    let payments = configured(&state)?;
    let company = linked_company(&state, &payments, &token).await?;
    let subscription = live_summary(&state.stripe, company.stripe_customer_id.as_deref()).await?;
    Ok(Json(PaymentPage {
        company_name: company.name,
        price_cents: company.subscription_price_cents,
        subscription,
    }))
}

/// A link to open in the browser.
#[derive(Debug, Serialize, ToSchema)]
pub struct StripeLink {
    /// Stripe's page (Checkout or the billing portal).
    pub url: String,
}

/// The company's Stripe customer, created and saved the first time.
async fn customer_of(state: &AppState, company: &CompanyRow) -> ApiResult<String> {
    if let Some(customer) = &company.stripe_customer_id {
        return Ok(customer.clone());
    }
    let created = state
        .stripe
        .create_customer(&company.name, company.id)
        .await
        .map_err(stripe_error)?;
    let created = created["id"]
        .as_str()
        .ok_or_else(|| ApiError::Upstream("Stripe returned no customer id".into()))?;
    // Only when no other request saved one first; then that one is used.
    sqlx::query(
        "UPDATE company SET stripe_customer_id = ?
          WHERE id = ? AND (stripe_customer_id IS NULL OR stripe_customer_id = '')",
    )
    .bind(created)
    .bind(company.id)
    .execute(&state.pool)
    .await?;
    let saved: Option<String> =
        sqlx::query_scalar("SELECT stripe_customer_id FROM company WHERE id = ?")
            .bind(company.id)
            .fetch_one(&state.pool)
            .await?;
    saved.ok_or_else(|| ApiError::Internal("the Stripe customer was not saved".into()))
}

/// Start a company's monthly subscription
///
/// A Stripe Checkout link for a monthly subscription at the company's price
/// (`subscription_price_cents`, read now; later price changes do not touch a
/// running subscription). The first time, a Stripe customer is created and
/// saved on the company. Stripe sends the payer back to the payments page
/// with `&checkout=success` or `&checkout=cancel`. 409 while the company has
/// a live subscription: open the portal instead.
#[crate::openapi::path(
    post,
    path = "/v1/public/payments/{token}/checkout",
    operation_id = "create_payment_checkout",
    tag = "payments",
    params(("token" = String, Path, description = "The `c` value of the company's payment link")),
    responses(
        (status = 200, description = "The Checkout link", body = StripeLink),
        (status = 404, description = "The link is not valid, or the company was deleted", body = ErrorBody),
        (status = 409, description = "The company already has a subscription", body = ErrorBody),
        (status = 502, description = "Stripe failed", body = ErrorBody),
        (status = 503, description = "Payments are not set up", body = ErrorBody),
    ),
)]
pub async fn start_checkout(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<Json<StripeLink>> {
    let payments = configured(&state)?;
    let company = linked_company(&state, &payments, &token).await?;
    if live_summary(&state.stripe, company.stripe_customer_id.as_deref())
        .await?
        .is_some()
    {
        return Err(ApiError::Conflict("You already have a subscription".into()));
    }
    let customer = customer_of(&state, &company).await?;
    state
        .stripe
        .ensure_product(PRODUCT_ID, PRODUCT_NAME, PRODUCT_TAX_CODE)
        .await
        .map_err(stripe_error)?;
    let page = payments
        .link(company.id)
        .ok_or_else(|| ApiError::Unavailable("Payments are not set up".into()))?;
    let session = state
        .stripe
        .create_monthly_checkout_session(
            &customer,
            company.id,
            PRODUCT_ID,
            i64::from(company.subscription_price_cents),
            &format!("{page}&checkout=success"),
            &format!("{page}&checkout=cancel"),
        )
        .await
        .map_err(stripe_error)?;
    let url = session["url"]
        .as_str()
        .ok_or_else(|| ApiError::Upstream("Stripe returned no Checkout link".into()))?;
    Ok(Json(StripeLink { url: url.into() }))
}

/// Manage a company's subscription in Stripe
///
/// A Stripe billing portal link where the company cancels its subscription
/// (at the end of the paid month), changes its card and downloads invoices.
/// Stripe sends it back to the payments page. 409 when the company never
/// subscribed.
#[crate::openapi::path(
    post,
    path = "/v1/public/payments/{token}/portal",
    operation_id = "create_payment_portal",
    tag = "payments",
    params(("token" = String, Path, description = "The `c` value of the company's payment link")),
    responses(
        (status = 200, description = "The billing portal link", body = StripeLink),
        (status = 404, description = "The link is not valid, or the company was deleted", body = ErrorBody),
        (status = 409, description = "The company has no Stripe customer yet", body = ErrorBody),
        (status = 502, description = "Stripe failed", body = ErrorBody),
        (status = 503, description = "Payments are not set up", body = ErrorBody),
    ),
)]
pub async fn open_portal(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<Json<StripeLink>> {
    let payments = configured(&state)?;
    let company = linked_company(&state, &payments, &token).await?;
    let customer = company
        .stripe_customer_id
        .ok_or_else(|| ApiError::Conflict("There is no subscription to manage yet".into()))?;
    let page = payments
        .link(company.id)
        .ok_or_else(|| ApiError::Unavailable("Payments are not set up".into()))?;
    let configuration = state
        .stripe
        .payments_portal_configuration()
        .await
        .map_err(stripe_error)?;
    let session = state
        .stripe
        .create_portal_session_with(&customer, &page, configuration.as_deref())
        .await
        .map_err(stripe_error)?;
    let url = session["url"]
        .as_str()
        .ok_or_else(|| ApiError::Upstream("Stripe returned no portal link".into()))?;
    Ok(Json(StripeLink { url: url.into() }))
}

/// A company on the superuser payments screen.
#[derive(Debug, Serialize, ToSchema)]
pub struct CompanyPayments {
    /// Company id.
    pub id: i32,
    /// Company name.
    pub name: String,
    /// The monthly price its next new subscription starts at, in US cents
    /// (default 80000 = $800). A running subscription keeps its own price.
    pub subscription_price_cents: i32,
    /// The payments page link to send the company.
    pub payment_link: String,
    /// Its Stripe customer (`cus_...`); null until its first checkout.
    pub stripe_customer_id: Option<String>,
    /// Its live subscription, else its latest ended one; null when it never
    /// subscribed.
    pub subscription: Option<SubscriptionSummary>,
    /// When its last successful payment was made; null when none.
    pub last_payment_at: Option<DateTime<Utc>>,
    /// What that payment was, in US cents.
    pub last_payment_cents: Option<i64>,
    /// Why Stripe could not be read for this company; null when it was.
    pub stripe_error: Option<String>,
}

/// The newest paid invoice with money in it: (paid at, cents).
fn last_payment(invoices: &[Value]) -> Option<(Option<DateTime<Utc>>, i64)> {
    invoices
        .iter()
        .filter(|invoice| invoice["status"] == "paid")
        .filter_map(|invoice| {
            let amount = invoice["amount_paid"]
                .as_i64()
                .filter(|amount| *amount > 0)?;
            let paid_at = unix(&invoice["status_transitions"]["paid_at"])
                .or_else(|| unix(&invoice["created"]));
            Some((paid_at, amount))
        })
        .max_by_key(|(paid_at, _)| *paid_at)
}

async fn company_payments(
    stripe: &Stripe,
    payments: &Payments,
    row: CompanyRow,
) -> CompanyPayments {
    let mut company = CompanyPayments {
        id: row.id,
        payment_link: payments.link(row.id).unwrap_or_default(),
        name: row.name,
        subscription_price_cents: row.subscription_price_cents,
        stripe_customer_id: row.stripe_customer_id,
        subscription: None,
        last_payment_at: None,
        last_payment_cents: None,
        stripe_error: None,
    };
    let Some(customer) = company.stripe_customer_id.as_deref() else {
        return company;
    };
    let (subscriptions, invoices) = tokio::join!(
        stripe.list_subscriptions(customer),
        stripe.list_invoices(customer)
    );
    match (subscriptions, invoices) {
        (Ok(subscriptions), Ok(invoices)) => {
            company.subscription = live_subscription(&subscriptions)
                .or_else(|| subscriptions.first())
                .map(SubscriptionSummary::from_stripe);
            if let Some((paid_at, cents)) = last_payment(&invoices) {
                company.last_payment_at = paid_at;
                company.last_payment_cents = Some(cents);
            }
        }
        (Err(error), _) | (_, Err(error)) => company.stripe_error = Some(error.to_string()),
    }
    company
}

/// List every company's payments
///
/// Every company (deleted ones hidden), by name: its monthly price, the
/// payments page link to send it, and from Stripe its subscription (live,
/// else the latest ended one), whether it renews and when, and its last
/// successful payment. A company whose Stripe data could not be read has
/// `stripe_error` set. Superusers only. Change a price with `PUT
/// /v1/superuser/companies/{id}/subscription-price`.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/payments",
    operation_id = "list_company_payments",
    tag = "superuser",
    responses(
        (status = 200, description = "Every company's payments", body = [CompanyPayments]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 503, description = "Payments are not set up", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_payments(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Vec<CompanyPayments>>> {
    if !user.is_superuser {
        return Err(ApiError::Forbidden);
    }
    let payments = configured(&state)?;
    let rows: Vec<CompanyRow> = sqlx::query_as(
        "SELECT id, COALESCE(NULLIF(TRIM(name), ''), CONCAT('Company ', id)) AS name,
                subscription_price_cents, NULLIF(stripe_customer_id, '') AS stripe_customer_id
           FROM company WHERE deleted_at IS NULL ORDER BY name, id",
    )
    .fetch_all(&state.pool)
    .await?;
    let stripe = &state.stripe;
    let payments = &*payments;
    let companies = stream::iter(rows)
        .map(|row| company_payments(stripe, payments, row))
        .buffered(STRIPE_CONCURRENCY)
        .collect()
        .await;
    Ok(Json(companies))
}

/// Body of `PUT /v1/superuser/companies/{id}/subscription-price`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SubscriptionPrice {
    /// The monthly price in US cents, 50 ($0.50) to 10000000 ($100,000).
    pub price_cents: i32,
}

/// Set a company's monthly price
///
/// The price the company's next new subscription starts at (read when it
/// checks out). A running subscription keeps the price it started with.
/// Superusers only.
#[crate::openapi::path(
    put,
    path = "/v1/superuser/companies/{id}/subscription-price",
    operation_id = "set_company_subscription_price",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    request_body = SubscriptionPrice,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Price out of range", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_subscription_price(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SubscriptionPrice>,
) -> ApiResult<StatusCode> {
    if !user.is_superuser {
        return Err(ApiError::Forbidden);
    }
    if !(MIN_PRICE_CENTS..=MAX_PRICE_CENTS).contains(&input.price_cents) {
        return Err(ApiError::BadRequest(
            "The price must be between $0.50 and $100,000".into(),
        ));
    }
    let updated = sqlx::query(
        "UPDATE company SET subscription_price_cents = ? WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(input.price_cents)
    .bind(id)
    .execute(&state.pool)
    .await?;
    if updated.rows_affected() == 0 {
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT id FROM company WHERE id = ? AND deleted_at IS NULL")
                .bind(id)
                .fetch_optional(&state.pool)
                .await?;
        exists.ok_or(ApiError::NotFound)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn links_name_their_company_and_cannot_be_forged() {
        let payments = Payments::new("secret", "https://payments.example/");
        let token = payments.token(42).unwrap();
        assert!(token.starts_with("42."));
        assert_eq!(token.len(), "42.".len() + 22);
        assert_eq!(payments.company_of(&token), Some(42));
        assert_eq!(
            payments.link(42).unwrap(),
            format!("https://payments.example/?c={token}")
        );
        // Another company's id with this signature, or another secret's.
        let signature = token.split_once('.').unwrap().1;
        assert_eq!(payments.company_of(&format!("43.{signature}")), None);
        assert_eq!(Payments::new("other", "").company_of(&token), None);
        // Tampered, truncated, padded or malformed.
        let mut tampered = token.clone();
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        for bad in [
            tampered.as_str(),
            &token[..token.len() - 1],
            &format!("{token}A"),
            &format!("042.{signature}"),
            &format!("+42.{signature}"),
            &format!("-42.{signature}"),
            "42",
            "42.",
            ".abc",
            "",
            "abc.def",
        ] {
            assert_eq!(payments.company_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn without_a_secret_there_are_no_links() {
        let payments = Payments::new("  ", "");
        assert!(!payments.is_enabled());
        assert_eq!(payments.token(1), None);
        assert_eq!(payments.link(1), None);
        assert_eq!(payments.company_of("1.abc"), None);
        assert_eq!(payments.app_url(), DEFAULT_APP_URL);
    }

    #[test]
    fn an_active_subscription_renews_on_its_period_end() {
        let subscription = json!({
            "id": "sub_1", "status": "active", "cancel_at_period_end": false, "cancel_at": null,
            "items": { "data": [{ "quantity": 1, "current_period_end": 1_800_000_000,
                                  "price": { "unit_amount": 80000 } }] },
        });
        assert_eq!(
            SubscriptionSummary::from_stripe(&subscription),
            SubscriptionSummary {
                id: "sub_1".into(),
                status: "active".into(),
                price_cents: Some(80000),
                renews: true,
                next_payment_at: DateTime::from_timestamp(1_800_000_000, 0),
                ends_at: None,
            }
        );
    }

    #[test]
    fn a_cancelling_subscription_ends_and_does_not_renew() {
        let at_period_end = json!({
            "id": "sub_1", "status": "active", "cancel_at_period_end": true, "cancel_at": null,
            "current_period_end": 1_800_000_000,
            "items": { "data": [{ "quantity": 1, "price": { "unit_amount": 50000 } }] },
        });
        let summary = SubscriptionSummary::from_stripe(&at_period_end);
        assert!(!summary.renews);
        assert_eq!(summary.next_payment_at, None);
        assert_eq!(summary.ends_at, DateTime::from_timestamp(1_800_000_000, 0));

        let on_a_date = json!({
            "id": "sub_2", "status": "trialing", "cancel_at_period_end": false, "cancel_at": 1_790_000_000,
            "items": { "data": [{ "current_period_end": 1_800_000_000, "price": { "unit_amount": 50000 } }] },
        });
        let summary = SubscriptionSummary::from_stripe(&on_a_date);
        assert!(!summary.renews);
        assert_eq!(summary.ends_at, DateTime::from_timestamp(1_790_000_000, 0));
    }

    #[test]
    fn ended_and_unpaid_subscriptions_do_not_renew() {
        for status in [
            "canceled",
            "unpaid",
            "incomplete",
            "incomplete_expired",
            "paused",
        ] {
            let subscription = json!({
                "id": "sub_1", "status": status, "cancel_at_period_end": false,
                "items": { "data": [{ "current_period_end": 1_800_000_000, "price": { "unit_amount": 100 } }] },
            });
            let summary = SubscriptionSummary::from_stripe(&subscription);
            assert!(!summary.renews, "{status}");
            assert_eq!(summary.next_payment_at, None, "{status}");
            assert_eq!(summary.ends_at, None, "{status}");
        }
        // A past-due subscription is still charged again.
        let past_due = json!({ "id": "s", "status": "past_due", "items": { "data": [{
            "current_period_end": 1_800_000_000, "price": { "unit_amount": 100 } }] } });
        assert!(SubscriptionSummary::from_stripe(&past_due).renews);
    }

    #[test]
    fn the_live_subscription_is_the_newest_not_ended_one() {
        let subscriptions = [
            json!({ "id": "sub_3", "status": "incomplete_expired" }),
            json!({ "id": "sub_2", "status": "past_due" }),
            json!({ "id": "sub_1", "status": "canceled" }),
        ];
        assert_eq!(live_subscription(&subscriptions).unwrap()["id"], "sub_2");
        assert!(live_subscription(&subscriptions[2..]).is_none());
        assert!(live_subscription(&[]).is_none());
    }

    #[test]
    fn the_last_payment_is_the_newest_paid_invoice_with_money() {
        let invoices = [
            json!({ "status": "open", "amount_paid": 0, "status_transitions": { "paid_at": null }, "created": 400 }),
            json!({ "status": "paid", "amount_paid": 0, "status_transitions": { "paid_at": 300 } }),
            json!({ "status": "paid", "amount_paid": 80000, "status_transitions": { "paid_at": 200 } }),
            json!({ "status": "paid", "amount_paid": 70000, "status_transitions": { "paid_at": 100 } }),
        ];
        assert_eq!(
            last_payment(&invoices),
            Some((DateTime::from_timestamp(200, 0), 80000))
        );
        assert_eq!(last_payment(&invoices[..2]), None);
    }
}
