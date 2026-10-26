//! The billing page (`/admin/billing`) and the access gate.
//!
//! - `GET  /v1/billing/gate`         → past-due warning / paused access, for every page
//! - `GET  /v1/billing`              → the subscription, card, plans and invoices (managers)
//! - `POST /v1/billing/checkout`     → a Stripe Checkout link to subscribe (managers)
//! - `POST /v1/billing/portal`       → a Stripe billing portal link (managers)
//! - `PUT  /v1/billing/subscription` → change the plan (managers)
//! - `GET  /v1/billing/reactivation` → what restoring a suspended company costs (managers)
//!
//! Replaces the Remix `/admin/billing` and `/admin/billing/pay` loaders and
//! action, and the root loader's billing gate.

use crate::auth::{BillingManager, CurrentUser};
use crate::billing::account::{self, Card};
use crate::billing::machine::BillingNotice;
use crate::billing::reactivation::{self, reactivation_plan};
use crate::billing::store::{BillingStore, MySqlBillingStore};
use crate::billing::{
    BillingInvoice, CompanyBilling, MANAGER_POSITION, can_start_checkout, needs_new_subscription,
};
use crate::error::{ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::{AppState, origin_allowed};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(gate))
        .routes(routes!(overview))
        .routes(routes!(checkout))
        .routes(routes!(portal))
        .routes(routes!(change_plan))
        .routes(routes!(reactivation_quote))
}

fn utc(date: Option<NaiveDateTime>) -> Option<DateTime<Utc>> {
    date.map(|date| date.and_utc())
}

/// `GET /v1/billing/gate`.
#[derive(Debug, Serialize, ToSchema)]
pub struct BillingGate {
    #[serde(flatten)]
    pub notice: BillingNotice,
    /// The caller is a manager of the company (manages billing).
    pub can_manage: bool,
}

/// Get the billing gate
///
/// Whether the company's access is paused for an unpaid subscription
/// (`restricted`: only the billing pages work) and whether managers see the
/// past-due warning with the grace days left. Checks Stripe now and then
/// (throttled); errors keep access.
#[crate::openapi::path(
    get,
    path = "/v1/billing/gate",
    operation_id = "get_billing_gate",
    tag = "billing",
    responses(
        (status = 200, description = "The gate", body = BillingGate),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn gate(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<BillingGate>> {
    let notice =
        reactivation::billing_gate(&state.stripe, &state.pool, user.company_id, user.id).await;
    let can_manage: bool = sqlx::query_scalar(
        "SELECT COUNT(*) > 0 FROM users_positions WHERE user_id = ? AND company_id = ? AND position_id = ?",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(MANAGER_POSITION)
    .fetch_one(&state.pool)
    .await?;
    Ok(Json(BillingGate { notice, can_manage }))
}

/// A plan the company can subscribe to.
#[derive(Debug, Serialize, ToSchema)]
pub struct Plan {
    /// Stripe price id (`price_...`).
    pub price_id: String,
    /// Plan name to show.
    pub name: String,
    /// Monthly price in cents, when configured.
    pub monthly_price_cents: Option<i64>,
}

/// The company's subscription as stored (refreshed from Stripe).
#[derive(Debug, Serialize, ToSchema)]
pub struct Subscription {
    /// Plan name, or null.
    pub plan_name: Option<String>,
    /// Stripe price id of the current plan, or null.
    pub price_id: Option<String>,
    /// Monthly price in cents, or null.
    pub monthly_price_cents: Option<i64>,
    /// `active`, `past_due`, `suspended` or `none`.
    pub payment_status: String,
    /// Stripe's subscription status (`active`, `canceled`, `incomplete`, ...).
    pub subscription_status: String,
    /// Next billing date (ISO 8601), or null.
    pub current_period_end: Option<DateTime<Utc>>,
    /// Last successful payment (ISO 8601), or null.
    pub last_successful_payment_at: Option<DateTime<Utc>>,
    /// Sum of open invoices, in cents.
    pub outstanding_balance_cents: i64,
}

/// An invoice of the company.
#[derive(Debug, Serialize, ToSchema)]
pub struct Invoice {
    /// Stripe invoice id (`in_...`).
    pub stripe_invoice_id: String,
    /// `paid` or `open`.
    pub status: String,
    /// What is still owed, in cents (0 once paid).
    pub amount_due_cents: i64,
    /// What was paid, in cents.
    pub amount_paid_cents: i64,
    /// Stripe's page to pay or view the invoice, or null.
    pub hosted_invoice_url: Option<String>,
    /// The invoice PDF, or null.
    pub invoice_pdf: Option<String>,
    /// When it was paid (ISO 8601), or null.
    pub paid_at: Option<DateTime<Utc>>,
}

impl From<BillingInvoice> for Invoice {
    fn from(invoice: BillingInvoice) -> Self {
        Self {
            stripe_invoice_id: invoice.stripe_invoice_id,
            status: invoice.status,
            amount_due_cents: invoice.amount_due_cents,
            amount_paid_cents: invoice.amount_paid_cents,
            hosted_invoice_url: invoice.hosted_invoice_url,
            invoice_pdf: invoice.invoice_pdf,
            paid_at: utc(invoice.paid_at),
        }
    }
}

fn subscription(record: &CompanyBilling) -> Subscription {
    Subscription {
        plan_name: record.plan_name.clone(),
        price_id: record.stripe_price_id.clone(),
        monthly_price_cents: record.monthly_price_cents,
        payment_status: record.payment_status.as_str().into(),
        subscription_status: record.subscription_status.clone(),
        current_period_end: utc(record.current_period_end),
        last_successful_payment_at: utc(record.last_successful_payment_at),
        outstanding_balance_cents: record.outstanding_balance_cents,
    }
}

fn plans(state: &AppState) -> Vec<Plan> {
    state
        .billing_plans
        .iter()
        .map(|plan| Plan {
            price_id: plan.price_id.clone(),
            name: plan.name.clone(),
            monthly_price_cents: plan.monthly_price_cents,
        })
        .collect()
}

/// `GET /v1/billing`.
#[derive(Debug, Serialize, ToSchema)]
pub struct BillingOverview {
    /// The subscription, or null when the company never had one.
    pub subscription: Option<Subscription>,
    /// The card subscriptions are charged to, or null.
    pub card: Option<Card>,
    /// Paid and open invoices, newest payment first.
    pub invoices: Vec<Invoice>,
    /// Plans to subscribe or switch to.
    pub plans: Vec<Plan>,
    /// Access is paused until the company pays.
    pub restricted: bool,
    /// The subscription ended (or never started): subscribing goes through
    /// Checkout (`POST /v1/billing/checkout`), not a plan change.
    pub starts_checkout: bool,
    /// Paused and without a running subscription: pay on the restore page
    /// (`GET /v1/billing/reactivation`).
    pub needs_new_subscription: bool,
    /// Stripe is configured on the server.
    pub billing_configured: bool,
}

/// Get the company's billing
///
/// The subscription (refreshed from Stripe first), the card on file, the
/// plans and the invoices. Managers only; other users read
/// `GET /v1/billing/gate`.
#[crate::openapi::path(
    get,
    path = "/v1/billing",
    operation_id = "get_billing",
    tag = "billing",
    responses(
        (status = 200, description = "The company's billing", body = BillingOverview),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn overview(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
) -> ApiResult<Json<BillingOverview>> {
    let record = account::sync_from_stripe(&state.stripe, &state.pool, user.company_id).await?;
    let gate =
        reactivation::billing_gate(&state.stripe, &state.pool, user.company_id, user.id).await;
    let store = MySqlBillingStore(&state.pool);
    let record = match record {
        Some(record) => Some(record),
        None => store.by_company(user.company_id).await?,
    };
    let invoices = match &record {
        Some(_) => store.invoices(user.company_id).await?,
        None => Vec::new(),
    };
    let card = account::default_card(
        &state.stripe,
        record
            .as_ref()
            .and_then(|r| r.stripe_payment_method_id.as_deref()),
    )
    .await;
    Ok(Json(BillingOverview {
        subscription: record.as_ref().map(subscription),
        card,
        invoices: invoices.into_iter().map(Invoice::from).collect(),
        plans: plans(&state),
        restricted: gate.restricted,
        starts_checkout: can_start_checkout(record.as_ref()),
        needs_new_subscription: gate.restricted && needs_new_subscription(record.as_ref()),
        billing_configured: state.stripe.is_enabled(),
    }))
}

/// The billing page on the host the request came from (the company's
/// subdomain), else on `APP_URL`.
fn billing_url(state: &AppState, headers: &HeaderMap) -> String {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .filter(|origin| origin_allowed(&state.allowed_origins, origin))
        .unwrap_or(&state.app_url);
    format!(
        "{}{}",
        origin.trim_end_matches('/'),
        crate::billing::BILLING_PATH
    )
}

/// A link to open in the browser.
#[derive(Debug, Serialize, ToSchema)]
pub struct Redirect {
    /// Stripe's page.
    pub url: String,
}

/// Body of `POST /v1/billing/checkout` and `PUT /v1/billing/subscription`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PlanChoice {
    /// A plan's `price_id` from `GET /v1/billing`.
    pub price_id: String,
}

/// Subscribe through Stripe Checkout
///
/// A Stripe Checkout link that subscribes the company to the plan; Stripe
/// returns to `/admin/billing?checkout=success` (or `cancel`). Only while the
/// company has no running subscription (400 otherwise: change the plan).
#[crate::openapi::path(
    post,
    path = "/v1/billing/checkout",
    operation_id = "create_billing_checkout",
    tag = "billing",
    request_body = PlanChoice,
    responses(
        (status = 200, description = "The Checkout link", body = Redirect),
        (status = 400, description = "Unknown plan, already subscribed, or Stripe refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn checkout(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
    headers: HeaderMap,
    JsonBody(input): JsonBody<PlanChoice>,
) -> ApiResult<Json<Redirect>> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM company WHERE id = ?")
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?
        .flatten();
    let name = name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Company".into());
    let url = account::checkout_url(
        &state.stripe,
        &MySqlBillingStore(&state.pool),
        &state.billing_plans,
        user.company_id,
        &name,
        input.price_id.trim(),
        &billing_url(&state, &headers),
    )
    .await?;
    Ok(Json(Redirect { url }))
}

/// Body of `POST /v1/billing/portal`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PortalChoice {
    /// `payment_method` (the card form, the default) or `manage` (the whole
    /// subscription).
    #[serde(default)]
    pub flow: Option<String>,
}

/// Open the Stripe billing portal
///
/// A Stripe billing portal link for the company's customer, returning to
/// `/admin/billing`.
#[crate::openapi::path(
    post,
    path = "/v1/billing/portal",
    operation_id = "create_billing_portal",
    tag = "billing",
    request_body = PortalChoice,
    responses(
        (status = 200, description = "The portal link", body = Redirect),
        (status = 400, description = "No Stripe customer, or Stripe refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn portal(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
    headers: HeaderMap,
    JsonBody(input): JsonBody<PortalChoice>,
) -> ApiResult<Json<Redirect>> {
    let url = account::portal_url(
        &state.stripe,
        &MySqlBillingStore(&state.pool),
        user.company_id,
        &billing_url(&state, &headers),
        input.flow.as_deref() != Some("manage"),
    )
    .await?;
    Ok(Json(Redirect { url }))
}

/// Change the plan
///
/// Moves the running subscription to another plan (prorated).
#[crate::openapi::path(
    put,
    path = "/v1/billing/subscription",
    operation_id = "change_billing_plan",
    tag = "billing",
    request_body = PlanChoice,
    responses(
        (status = 204, description = "Changed"),
        (status = 400, description = "Unknown plan, no running subscription, or Stripe refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn change_plan(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
    JsonBody(input): JsonBody<PlanChoice>,
) -> ApiResult<StatusCode> {
    account::update_plan(
        &state.stripe,
        &MySqlBillingStore(&state.pool),
        &state.billing_plans,
        user.company_id,
        input.price_id.trim(),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// What restoring access costs.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReactivationPlan {
    /// Stripe price id (`price_...`).
    pub price_id: String,
    /// Plan name to show.
    pub plan_name: String,
    /// Units billed (always 1: a flat price per company).
    pub quantity: i64,
    /// Due today and each month, in cents; null when unknown.
    pub monthly_cents: Option<i64>,
}

/// `GET /v1/billing/reactivation`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReactivationQuote {
    /// The company is paused without a running subscription: paying restores
    /// it. False: go back to `/admin/billing`.
    pub needed: bool,
    /// Saved cards, the default first.
    pub cards: Vec<Card>,
    /// The plan to pay for; null when no plan is configured.
    pub plan: Option<ReactivationPlan>,
    /// Stripe.js publishable key (`pk_...`), or null when not configured.
    pub publishable_key: Option<String>,
}

/// Get what restoring access costs
///
/// For a suspended company: the plan and price to pay, the saved cards and
/// the Stripe.js key; then pay with `POST /v1/billing/reactivation`.
#[crate::openapi::path(
    get,
    path = "/v1/billing/reactivation",
    operation_id = "get_billing_reactivation",
    tag = "billing",
    responses(
        (status = 200, description = "The quote", body = ReactivationQuote),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn reactivation_quote(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
) -> ApiResult<Json<ReactivationQuote>> {
    let record = account::sync_from_stripe(&state.stripe, &state.pool, user.company_id).await?;
    let gate =
        reactivation::billing_gate(&state.stripe, &state.pool, user.company_id, user.id).await;
    let needed = gate.restricted && needs_new_subscription(record.as_ref());
    if !needed {
        return Ok(Json(ReactivationQuote {
            needed,
            cards: Vec::new(),
            plan: None,
            publishable_key: None,
        }));
    }
    let key = std::env::var("STRIPE_PUBLIC_KEY").unwrap_or_default();
    Ok(Json(ReactivationQuote {
        needed,
        cards: account::saved_cards(&state.stripe, record.as_ref()).await,
        plan: reactivation_plan(record.as_ref(), &state.billing_plans).map(|plan| {
            ReactivationPlan {
                price_id: plan.price_id,
                plan_name: plan.plan_name,
                quantity: plan.quantity,
                monthly_cents: plan.monthly_cents,
            }
        }),
        publishable_key: Some(key.trim().to_string()).filter(|key| key.starts_with("pk_")),
    }))
}
