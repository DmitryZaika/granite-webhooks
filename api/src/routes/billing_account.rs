//! The billing page (`/admin/billing`) and the access gate.
//!
//! - `GET  /v1/billing/gate`         → past-due warning / paused access, for every page
//! - `GET  /v1/billing`              → the subscription, card, plans and invoices (managers)
//! - `POST /v1/billing/checkout`     → refused: plans come through a plan invite (managers)
//! - `POST /v1/billing/plan-invite/link` → the caller's link to the open plan invite (managers)
//! - `POST /v1/billing/portal`       → a Stripe billing portal link (managers)
//! - `PUT  /v1/billing/subscription` → change the plan (managers)
//! - `GET  /v1/billing/reactivation` → what restoring a suspended company costs (managers)
//!
//! Replaces the Remix `/admin/billing` and `/admin/billing/pay` loaders and
//! action, and the root loader's billing gate.

use crate::auth::{BillingManager, CurrentUser};
use crate::billing::account::{self, Card, PageInvoice, SyncOptions};
use crate::billing::gate::{self, BillingNotice};
use crate::billing::invites;
use crate::billing::reactivation::{load_billing_plans, reactivation_plan};
use crate::billing::store::{BillingStore, MySqlBillingStore};
use crate::billing::{CompanyBilling, MANAGER_POSITION, needs_new_subscription};
use crate::error::{ApiError, ApiResult, CodedErrorBody, ErrorBody};
use crate::extract::JsonBody;
use crate::state::{AppState, origin_allowed};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(gate))
        .routes(routes!(overview))
        .routes(routes!(checkout))
        .routes(routes!(plan_invite_link))
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
/// Whether the company's access is paused (`restricted`: other routes answer
/// 402 `payment_required`, only billing, sign-in, the app shell, `GET /v1/me`
/// and notifications work) and whether managers see a warning with the days
/// left. `reason` says why: `payment_failed` (a payment failed, 7 days of
/// grace) or `subscription_required` (payment was turned on and the company
/// has not subscribed, 7 days to do so); `pauses_at` is when access pauses.
/// Checks Stripe now and then (throttled); errors keep access.
#[utoipa::path(
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
    let notice = Box::pin(gate::billing_gate(
        &state.stripe,
        &state.pool,
        user.company_id,
        user.id,
    ))
    .await;
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
    /// Monthly price in cents: what Stripe charges for the price, else the
    /// configured amount; null when neither is known.
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
    /// When the payment of the current dunning cycle failed (ISO 8601); null
    /// when no payment is failing.
    pub failed_payment_at: Option<DateTime<Utc>>,
    /// Sum of open invoices, in cents.
    pub outstanding_balance_cents: i64,
}

/// An invoice of the company.
#[derive(Debug, Serialize, ToSchema)]
pub struct Invoice {
    /// Stripe invoice id (`in_...`).
    pub stripe_invoice_id: String,
    /// Stripe's invoice number to show (e.g. `GM-0001`); null when Stripe
    /// could not be reached (the stored copy has none).
    pub number: Option<String>,
    /// `paid`, `open`, `void` or `uncollectible` (the stored copy, used when
    /// Stripe cannot be reached, has `paid` and `open` only).
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
    /// When Stripe issued it (ISO 8601); null when Stripe could not be
    /// reached. Show `paid_at` for paid invoices, else this.
    pub created_at: Option<DateTime<Utc>>,
}

impl From<PageInvoice> for Invoice {
    fn from(invoice: PageInvoice) -> Self {
        Self {
            stripe_invoice_id: invoice.stripe_invoice_id,
            number: invoice.number,
            status: invoice.status,
            amount_due_cents: invoice.amount_due_cents,
            amount_paid_cents: invoice.amount_paid_cents,
            hosted_invoice_url: invoice.hosted_invoice_url,
            invoice_pdf: invoice.invoice_pdf,
            paid_at: utc(invoice.paid_at),
            created_at: utc(invoice.created_at),
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
        failed_payment_at: utc(record.failed_payment_at),
        outstanding_balance_cents: record.outstanding_balance_cents,
    }
}

async fn plans(state: &AppState) -> Vec<Plan> {
    load_billing_plans(&state.stripe, &state.billing_plans)
        .await
        .into_iter()
        .map(|plan| Plan {
            price_id: plan.price_id,
            name: plan.name,
            monthly_price_cents: plan.monthly_price_cents,
        })
        .collect()
}

/// `GET /v1/billing`.
#[derive(Debug, Serialize, ToSchema)]
pub struct BillingOverview {
    /// The subscription, or null when the company never had one.
    pub subscription: Option<Subscription>,
    /// The card renewals are charged to (the subscription's, else the
    /// customer's default), or null.
    pub card: Option<Card>,
    /// The latest invoices (up to 24, no drafts), newest first.
    pub invoices: Vec<Invoice>,
    /// Plans to subscribe or switch to.
    pub plans: Vec<Plan>,
    /// Access is paused until the company pays.
    pub restricted: bool,
    /// Always false: the flat-price Checkout no longer starts subscriptions.
    /// A company without a plan accepts a plan invite (`plan_invite`), or
    /// contacts Granite Manager.
    pub starts_checkout: bool,
    /// The plan invite a Manager can accept now, or null (then "Contact
    /// Granite Manager to choose your plan").
    pub plan_invite: Option<OpenPlanInvite>,
    /// Paused and without a running subscription: pay on the restore page
    /// (`GET /v1/billing/reactivation`).
    pub needs_new_subscription: bool,
    /// Stripe is configured on the server.
    pub billing_configured: bool,
}

/// A plan invite waiting for the company's Managers.
#[derive(Debug, Serialize, ToSchema)]
pub struct OpenPlanInvite {
    /// When its links stop working (ISO 8601, UTC).
    pub expires_at: DateTime<Utc>,
    /// The plan's first charge day (`YYYY-MM-DD`).
    pub first_charge_on: NaiveDate,
}

/// Query of `GET /v1/billing`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OverviewQuery {
    /// `success` when Stripe Checkout sent the manager back: the
    /// subscription is read from Stripe now, as its webhook may still be on
    /// the way. Any other value is ignored.
    pub checkout: Option<String>,
}

/// Get the company's billing
///
/// The subscription (refreshed from Stripe at most every 5 minutes, or now
/// with `checkout=success`), the card on file, the plans with Stripe's
/// prices, the invoices (read live from Stripe, else the stored copy) and
/// the open plan invite (get its link with `create_billing_plan_invite_link`).
/// Managers only; other users read `GET /v1/billing/gate`.
#[utoipa::path(
    get,
    path = "/v1/billing",
    operation_id = "get_billing",
    tag = "billing",
    params(OverviewQuery),
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
    Query(query): Query<OverviewQuery>,
) -> ApiResult<Json<BillingOverview>> {
    let options = SyncOptions {
        force: query.checkout.as_deref() == Some("success"),
        ..SyncOptions::default()
    };
    let record =
        account::sync_from_stripe(&state.stripe, &state.pool, user.company_id, options).await?;
    let gate = Box::pin(gate::billing_gate(
        &state.stripe,
        &state.pool,
        user.company_id,
        user.id,
    ))
    .await;
    let record = match record {
        Some(record) => Some(record),
        None => {
            MySqlBillingStore(&state.pool)
                .by_company(user.company_id)
                .await?
        }
    };
    let invoices = account::page_invoices(&state.stripe, &state.pool, record.as_ref()).await?;
    let card = account::default_card(&state.stripe, record.as_ref()).await;
    let plan_invite = invites::open_for_company(&state.pool, user.company_id)
        .await?
        .map(|invite| OpenPlanInvite {
            expires_at: invite.expires_at.and_utc(),
            first_charge_on: invite.first_charge_on,
        });
    Ok(Json(BillingOverview {
        subscription: record.as_ref().map(subscription),
        card,
        invoices: invoices.into_iter().map(Invoice::from).collect(),
        plans: plans(&state).await,
        restricted: gate.restricted,
        starts_checkout: false,
        plan_invite,
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
    /// Stripe's page, or the payments app's plan invite page.
    pub url: String,
}

/// Body of `PUT /v1/billing/subscription`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PlanChoice {
    /// A plan's `price_id` from `GET /v1/billing`.
    pub price_id: String,
}

/// Subscribe through Stripe Checkout (retired)
///
/// Always refused with 409 `plan_invite_required` ("Contact Granite Manager
/// to choose your plan."): the flat-price Checkout no longer starts
/// subscriptions. A company moves onto a plan by accepting a plan invite
/// (`plan_invite` of `get_billing`, link from
/// `create_billing_plan_invite_link`). Changing the plan of a running
/// legacy subscription (`PUT /v1/billing/subscription`) and restoring a
/// paused company (`/v1/billing/reactivation`) still work.
#[utoipa::path(
    post,
    path = "/v1/billing/checkout",
    operation_id = "create_billing_checkout",
    tag = "billing",
    responses(
        (status = 200, description = "Never: the Checkout link is no longer made", body = Redirect),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 409, description = "Plans come through a plan invite (code plan_invite_required)", body = CodedErrorBody),
    ),
    security(("session_cookie" = []))
)]
#[allow(clippy::unused_async)] // an axum handler
pub async fn checkout(BillingManager(_user): BillingManager) -> ApiResult<Json<Redirect>> {
    Err(ApiError::coded(
        StatusCode::CONFLICT,
        "plan_invite_required",
        "Contact Granite Manager to choose your plan.",
    ))
}

/// Get my link to the open plan invite
///
/// Makes a new link for the caller to the company's open plan invite (the
/// payments app's `/upgrade/<token>`, where a Manager reviews and accepts
/// the plan). 404 `invite_not_found` when no invite is open.
#[utoipa::path(
    post,
    path = "/v1/billing/plan-invite/link",
    operation_id = "create_billing_plan_invite_link",
    tag = "billing",
    responses(
        (status = 200, description = "The link", body = Redirect),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 404, description = "No open plan invite (code invite_not_found)", body = CodedErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn plan_invite_link(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
) -> ApiResult<Json<Redirect>> {
    let invite = invites::open_for_company(&state.pool, user.company_id)
        .await?
        .ok_or_else(invites::not_found)?;
    let url = invites::mint_link(&state.pool, &invite.id, user.id).await?;
    Ok(Json(Redirect { url }))
}

/// Body of `POST /v1/billing/portal`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PortalChoice {
    /// `payment_method` (the card form, the default) or `manage` (the whole
    /// subscription).
    #[serde(default)]
    pub flow: Option<String>,
    /// Where the portal sends the manager back: `account` (the payments
    /// app's account page), else `/admin/billing` (the default).
    #[serde(default)]
    pub return_to: Option<String>,
}

/// Open the Stripe billing portal
///
/// A Stripe billing portal link for the company's customer, returning to
/// `/admin/billing`, or to the payments app's account page with
/// `return_to: "account"`. The portal updates cards and customer details and
/// lists invoices; it cannot cancel or change the plan.
#[utoipa::path(
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
    let return_url = if input.return_to.as_deref() == Some("account") {
        crate::billing::payments_account_url()
    } else {
        billing_url(&state, &headers)
    };
    let url = account::portal_url(
        &state.stripe,
        &MySqlBillingStore(&state.pool),
        user.company_id,
        &return_url,
        input.flow.as_deref() != Some("manage"),
    )
    .await?;
    Ok(Json(Redirect { url }))
}

/// Change the plan
///
/// Moves the running subscription to another plan (prorated).
#[utoipa::path(
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
#[utoipa::path(
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
    let record = account::sync_from_stripe(
        &state.stripe,
        &state.pool,
        user.company_id,
        SyncOptions::default(),
    )
    .await?;
    let gate = Box::pin(gate::billing_gate(
        &state.stripe,
        &state.pool,
        user.company_id,
        user.id,
    ))
    .await;
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
    let plans = load_billing_plans(&state.stripe, &state.billing_plans).await;
    Ok(Json(ReactivationQuote {
        needed,
        cards: account::saved_cards(&state.stripe, record.as_ref()).await,
        plan: reactivation_plan(record.as_ref(), &plans).map(|plan| ReactivationPlan {
            price_id: plan.price_id,
            plan_name: plan.plan_name,
            quantity: plan.quantity,
            monthly_cents: plan.monthly_cents,
        }),
        publishable_key: Some(key.trim().to_string()).filter(|key| key.starts_with("pk_")),
    }))
}
