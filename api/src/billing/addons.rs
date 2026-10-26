//! The payments app's account page and the add-ons of a self-serve
//! subscription.
//!
//! The page reads the live subscription from Stripe: its items by lookup key
//! (`catalog::PriceKind`), the next invoice preview and the payment method.
//! When Stripe cannot be read it falls back to `company_billing` and
//! `company_addons`, so the page never fails because Stripe is down. Adding
//! or removing an add-on re-reads the live subscription under the company's
//! billing lock, changes its item (prorated) and writes `company_addons` at
//! once; the webhook item sync (`items.rs`) confirms. Add-on items are
//! created with the idempotency key `addon:<company_id>:<key>:<marker>`, the
//! marker being when the previous item of that add-on ended (`first` before
//! any), so a retry replays and a later add creates.

use super::catalog::{
    BASE_LOOKUP_KEY, Catalog, CatalogError, EXTRA_USER_LOOKUP_KEY, INCLUDED_USERS,
};
use super::lock::{BUSY_MESSAGE, LockError, with_company_lock};
use super::seats;
use super::store::{BillingStore, MySqlBillingStore};
use super::stripe::{Stripe, StripeError, subscription_snapshot};
use super::{
    CompanyBilling, ItemSnapshot, SubscriptionSnapshot, is_ended_subscription_status, is_overdue,
    with_grace_expiry,
};
use super::{account, cancellation};
use crate::entitlements::{ACTIVE_ADDON, AddonKey};
use crate::routes::billing_account::Invoice;
use crate::routes::billing_cancellation::CancellationRequestBody;
use crate::state::AppState;
use chrono::{DateTime, NaiveDateTime, Utc};
use lambda_http::tracing;
use serde::Serialize;
use serde_json::Value;
use sqlx::MySqlPool;
use std::time::Duration;
use utoipa::ToSchema;

/// Invoices the page lists.
const INVOICE_LIMIT: usize = 12;
pub const SUPPORT_EMAIL: &str = "support@granite-manager.com";
const LEGACY_PLAN: &str = "Your plan is set up by Granite Manager, so add-ons are not changed here. Contact support@granite-manager.com to change them.";
const NO_SUBSCRIPTION: &str = "There is no running subscription to change. Contact support@granite-manager.com if this looks wrong.";
pub const PAY_FIRST: &str = "Pay the open invoice first, then add add-ons.";

/// What an add-on does, as the public plans list says it.
pub const fn description(key: AddonKey) -> &'static str {
    match key {
        AddonKey::Signatures => "eSignatures and an email signature block",
        AddonKey::Quotes => "Quote editor, price lists and online quotes",
        AddonKey::Quickbooks => "Invoices and QuickBooks sync",
    }
}

/// `GET /v1/billing/subscription`: the company's subscription as the
/// payments app's account page shows it. Money in cents, before sales tax;
/// times are ISO 8601 UTC.
#[derive(Debug, Serialize, ToSchema)]
pub struct AccountSubscription {
    /// The company's name.
    pub company_name: String,
    /// `subscription` (self-serve, managed on this page) or `legacy`
    /// (billed by Granite Manager: every other field is null, empty or
    /// false, `status` and `payment_status` are `none`; link to
    /// `/admin/billing` instead).
    pub plan_source: String,
    /// Stripe's subscription status: `trialing` (the free period), `active`,
    /// `past_due`, `unpaid`, `canceled`, `incomplete`, ...; `none` when the
    /// company has no subscription.
    pub status: String,
    /// `active` (paid up), `past_due` (a payment failed, the grace period
    /// runs), `suspended` (access paused until paid) or `none` (no payment
    /// yet).
    pub payment_status: String,
    /// When the free period ends (or ended); null when there was none.
    pub trial_end_at: Option<DateTime<Utc>>,
    /// End of the current billing period; null when unknown.
    pub current_period_end: Option<DateTime<Utc>>,
    /// The next charge: the end of the free period while trialing, else the
    /// end of the current period. Null when a cancellation is scheduled or
    /// the subscription ended.
    pub next_charge_at: Option<DateTime<Utc>>,
    /// The base plan; null for legacy, or when Stripe's price cannot be read.
    pub base: Option<AccountBasePlan>,
    /// Billable users against the included ones; null for legacy.
    pub users: Option<AccountUsers>,
    /// Every add-on (signatures, quotes, quickbooks) with its state; empty
    /// for legacy.
    pub addons: Vec<AccountAddon>,
    /// Monthly total before tax: base, extra users and active add-ons as
    /// Stripe bills them now (the stored total when Stripe cannot be read).
    /// Null when unknown or legacy.
    pub monthly_total_cents: Option<i64>,
    /// Stripe's draft of the next invoice; null when there is none (a
    /// cancellation is scheduled) or Stripe cannot be read.
    pub next_invoice: Option<AccountNextInvoice>,
    /// What the next charge is paid with; null when none is saved or Stripe
    /// cannot be read (see `payment_method_known`).
    pub payment_method: Option<AccountPaymentMethod>,
    /// Stripe answered for the payment method: a null `payment_method` then
    /// means none is saved. False when Stripe could not be read: say it could
    /// not be loaded, never that none is on file.
    pub payment_method_known: bool,
    /// The scheduled cancellation request; null when none is scheduled
    /// (`POST /v1/billing/cancellation-request` schedules one).
    pub cancellation: Option<CancellationRequestBody>,
    /// The latest invoices (up to 12, no drafts), newest first; from Stripe,
    /// else the stored copy.
    pub invoices: Vec<Invoice>,
    /// Add-ons can be changed here (`PUT /v1/billing/addons/{key}`): a
    /// self-serve company with a running subscription (`trialing`, `active`
    /// or `past_due`), with Stripe configured and its prices readable.
    pub can_change: bool,
    /// The subscription can be cancelled here
    /// (`POST /v1/billing/cancellation-request`): a self-serve company with a
    /// running subscription (`trialing`, `active` or `past_due`), even while
    /// Stripe or its prices cannot be read.
    pub can_cancel: bool,
}

impl AccountSubscription {
    fn legacy(company_name: String) -> Self {
        Self {
            company_name,
            plan_source: "legacy".into(),
            status: "none".into(),
            payment_status: "none".into(),
            trial_end_at: None,
            current_period_end: None,
            next_charge_at: None,
            base: None,
            users: None,
            addons: Vec::new(),
            monthly_total_cents: None,
            next_invoice: None,
            payment_method: None,
            payment_method_known: false,
            cancellation: None,
            invoices: Vec::new(),
            can_change: false,
            can_cancel: false,
        }
    }
}

/// The base plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct AccountBasePlan {
    /// Name to show ("Base plan").
    pub label: String,
    /// Monthly price in cents.
    pub monthly_cents: i64,
    /// Users the base price covers (10).
    pub included_users: i64,
}

/// Users the company is billed for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct AccountUsers {
    /// Active users that count (not Check-in-only, the Walk-in user or
    /// superusers).
    pub billable: i64,
    /// Users the base price covers (10).
    pub included: i64,
    /// Billable users above the included ones.
    pub extra: i64,
    /// Monthly price of one extra user in cents; null when Stripe's price
    /// cannot be read.
    pub extra_unit_cents: Option<i64>,
    /// `extra` x `extra_unit_cents`; null when the price is unknown.
    pub extra_monthly_cents: Option<i64>,
}

/// Where an add-on stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AddonStatus {
    /// On the subscription: billed each month.
    Active,
    /// Not on the plan: can be added.
    Available,
    /// Granted free by Granite Manager; cannot be changed here.
    Included,
}

/// One add-on of the account page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct AccountAddon {
    /// `signatures`, `quotes` or `quickbooks`.
    pub key: AddonKey,
    /// Name to show.
    pub label: String,
    /// What it does.
    pub description: String,
    /// Monthly price in cents; null when Stripe's price cannot be read.
    pub monthly_cents: Option<i64>,
    /// `active`, `available` or `included`.
    pub status: AddonStatus,
    /// Estimate for the next invoice if it changes now: the prorated charge
    /// for the rest of the current period when added, the credit when
    /// removed. Linear by the seconds left in the period, rounded to the
    /// cent; 0 during the free period and for `included`; null when the price
    /// is unknown.
    pub prorated_cents: Option<i64>,
}

/// Stripe's draft of the next invoice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct AccountNextInvoice {
    /// When Stripe will charge it; null when Stripe does not say.
    pub date: Option<DateTime<Utc>>,
    /// Before tax, in cents.
    pub subtotal_cents: i64,
    /// Sales tax, in cents (0 when none).
    pub tax_cents: i64,
    /// With tax, in cents.
    pub total_cents: i64,
    /// What will be charged after credits, in cents.
    pub amount_due_cents: i64,
    /// Its lines as Stripe words them (prorations included).
    pub lines: Vec<AccountInvoiceLine>,
}

/// One line of the next invoice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct AccountInvoiceLine {
    /// Stripe's description, e.g. "Quotes (Jan 21 to Feb 21)".
    pub description: String,
    /// Amount in cents; negative for a credit.
    pub amount_cents: i64,
}

/// The payment method of the next charge (never the full number).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct AccountPaymentMethod {
    /// `card`, `us_bank_account`, or another Stripe payment method type
    /// (then only this field is set).
    #[serde(rename = "type")]
    pub kind: String,
    /// Card brand (`visa`, `mastercard`, ...); null for a bank account.
    pub brand: Option<String>,
    /// Last four digits of the card or account number.
    pub last4: Option<String>,
    /// Card expiry month (1-12); null for a bank account.
    pub exp_month: Option<i64>,
    /// Card expiry year (e.g. 2030); null for a bank account.
    pub exp_year: Option<i64>,
    /// The bank's name; null for a card.
    pub bank_name: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AddonError {
    /// The change is not possible; the message says why, for the user.
    #[error("{0}")]
    Refused(String),
    #[error("{BUSY_MESSAGE}")]
    Busy,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Stripe(#[from] StripeError),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
}

impl AddonError {
    /// Safe to log: no Stripe message text.
    pub fn reason(&self) -> String {
        match self {
            Self::Refused(message) => message.clone(),
            Self::Busy => "busy".into(),
            Self::Database(_) => "database error".into(),
            Self::Stripe(err) => format!("stripe {}", err.code()),
            Self::Catalog(err) => err.reason(),
        }
    }
}

impl From<LockError> for AddonError {
    fn from(err: LockError) -> Self {
        match err {
            LockError::Busy => Self::Busy,
            LockError::Database(err) => Self::Database(err),
        }
    }
}

fn refused(message: &str) -> AddonError {
    AddonError::Refused(message.into())
}

/// Estimate for adding (or the credit for removing) an add-on of
/// `monthly_cents` at `now`: linear by the seconds left in the period,
/// rounded to the cent. 0 while trialing (Stripe prorates the free period
/// to $0), and when the period is unknown or over.
pub fn prorated_cents(
    monthly_cents: i64,
    now: NaiveDateTime,
    period_start: Option<NaiveDateTime>,
    period_end: Option<NaiveDateTime>,
    trialing: bool,
) -> i64 {
    let (Some(start), Some(end)) = (period_start, period_end) else {
        return 0;
    };
    if trialing || monthly_cents <= 0 {
        return 0;
    }
    let period = (end - start).num_seconds();
    if period <= 0 {
        return 0;
    }
    let left = (end - now).num_seconds().clamp(0, period);
    let (monthly, left, period) = (
        i128::from(monthly_cents),
        i128::from(left),
        i128::from(period),
    );
    i64::try_from((monthly * left * 2 + period) / (period * 2)).unwrap_or(monthly_cents)
}

/// Comp wins over the subscription: Granite Manager granted it free.
pub const fn addon_status(comp: bool, on_subscription: bool) -> AddonStatus {
    if comp {
        AddonStatus::Included
    } else if on_subscription {
        AddonStatus::Active
    } else {
        AddonStatus::Available
    }
}

/// A subscription that can still be changed.
fn is_running(status: &str) -> bool {
    matches!(status, "trialing" | "active" | "past_due")
}

fn unix(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(value.as_i64()?, 0)
}

fn utc(date: Option<NaiveDateTime>) -> Option<DateTime<Utc>> {
    date.map(|date| date.and_utc())
}

/// Stripe's invoice preview (`invoices/create_preview`) for the page.
pub fn next_invoice(preview: &Value) -> AccountNextInvoice {
    let cents = |field: &str| preview[field].as_i64().unwrap_or_default();
    let tax_cents = preview["total_taxes"].as_array().map_or(0, |taxes| {
        taxes.iter().filter_map(|tax| tax["amount"].as_i64()).sum()
    });
    let lines = preview["lines"]["data"]
        .as_array()
        .map(|lines| {
            lines
                .iter()
                .map(|line| AccountInvoiceLine {
                    description: line["description"].as_str().unwrap_or_default().to_string(),
                    amount_cents: line["amount"].as_i64().unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    AccountNextInvoice {
        date: unix(&preview["next_payment_attempt"]).or_else(|| unix(&preview["period_end"])),
        subtotal_cents: cents("subtotal"),
        tax_cents,
        total_cents: cents("total"),
        amount_due_cents: cents("amount_due"),
        lines,
    }
}

/// An expanded Stripe payment method, without its secrets.
pub fn payment_method(method: &Value) -> Option<AccountPaymentMethod> {
    let kind = method["type"].as_str().filter(|kind| !kind.is_empty())?;
    let text = |value: &Value| value.as_str().map(str::to_string);
    let mut summary = AccountPaymentMethod {
        kind: kind.to_string(),
        brand: None,
        last4: None,
        exp_month: None,
        exp_year: None,
        bank_name: None,
    };
    match kind {
        "card" => {
            let card = &method["card"];
            summary.brand = text(&card["brand"]);
            summary.last4 = text(&card["last4"]);
            summary.exp_month = card["exp_month"].as_i64();
            summary.exp_year = card["exp_year"].as_i64();
        }
        "us_bank_account" => {
            let bank = &method["us_bank_account"];
            summary.bank_name = text(&bank["bank_name"]);
            summary.last4 = text(&bank["last4"]);
        }
        _ => {}
    }
    Some(summary)
}

/// The idempotency key marker of the next add: when the add-on's previous
/// subscription item ended, `first` before any. A row Stripe no longer has
/// but that never ended here uses its start.
fn add_marker(previous: Option<&PreviousAddon>) -> String {
    match previous {
        None => "first".into(),
        Some(PreviousAddon {
            ended_at: Some(ended),
            ..
        }) => ended.and_utc().timestamp().to_string(),
        Some(PreviousAddon { started_at, .. }) => format!("s{}", started_at.and_utc().timestamp()),
    }
}

/// A Stripe 400 for an idempotency key reused with other parameters.
fn is_key_reuse(err: &StripeError) -> bool {
    matches!(err, StripeError::Api(failure)
        if failure.code == "idempotency_error"
            || failure.message.starts_with("Keys for idempotent requests"))
}

fn addon_item(snapshot: &SubscriptionSnapshot, key: AddonKey) -> Option<&ItemSnapshot> {
    item_with(snapshot, key.lookup_key())
}

fn item_with<'a>(snapshot: &'a SubscriptionSnapshot, lookup_key: &str) -> Option<&'a ItemSnapshot> {
    snapshot
        .items
        .iter()
        .find(|item| item.lookup_key.as_deref() == Some(lookup_key))
}

#[derive(Debug, sqlx::FromRow)]
struct CompanyRow {
    name: Option<String>,
    plan_source: String,
}

async fn company(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Option<CompanyRow>> {
    sqlx::query_as(
        "SELECT name, CAST(plan_source AS CHAR) AS plan_source FROM company WHERE id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// The company's add-on rows in force now: (key, source).
async fn active_addons(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<(String, String)>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT ca.addon_key, CAST(ca.source AS CHAR)
           FROM company_addons ca
          WHERE ca.company_id = ? AND {ACTIVE_ADDON}"
    )))
    .bind(company_id)
    .fetch_all(pool)
    .await
}

fn has_row(rows: &[(String, String)], key: AddonKey, source: &str) -> bool {
    rows.iter()
        .any(|(addon, from)| addon == key.as_str() && from == source)
}

/// How long one Stripe read of the account page may take.
const PAGE_STRIPE_TIMEOUT: Duration = Duration::from_secs(5);

/// The account page's Stripe client: a slow Stripe answers the page from the
/// stored rows instead of holding it.
fn page_stripe(stripe: &Stripe) -> Stripe {
    let mut stripe = stripe.clone();
    stripe.request_timeout = PAGE_STRIPE_TIMEOUT;
    stripe
}

/// The subscription as Stripe has it now, raw and read; `None` when Stripe
/// is off or refuses the read, `Err` when Stripe cannot be reached (down,
/// throttling or timing out).
async fn live_subscription(
    stripe: &Stripe,
    company_id: i32,
    record: Option<&CompanyBilling>,
) -> Result<Option<(Value, SubscriptionSnapshot)>, StripeError> {
    let Some(id) = record.and_then(|r| r.stripe_subscription_id.as_deref()) else {
        return Ok(None);
    };
    if !stripe.is_enabled() {
        return Ok(None);
    }
    match stripe.subscription(id).await {
        Ok(raw) => {
            let snapshot = subscription_snapshot(&raw);
            Ok(Some((raw, snapshot)))
        }
        Err(err) => {
            tracing::warn!(
                company_id,
                error = err.code(),
                "account page: subscription read failed"
            );
            if err.is_retryable() {
                Err(err)
            } else {
                Ok(None)
            }
        }
    }
}

/// The subscription's default payment method, else the customer's.
/// The payment method on file; `Err` when Stripe could not tell (not
/// configured, or the customer could not be read).
async fn current_payment_method(
    stripe: &Stripe,
    subscription: Option<&Value>,
    customer: Option<&str>,
) -> Result<Option<AccountPaymentMethod>, StripeError> {
    if !stripe.is_enabled() {
        return Err(StripeError::NotConfigured);
    }
    if let Some(default) = subscription.map(|raw| &raw["default_payment_method"]) {
        if let Some(method) = payment_method(default) {
            return Ok(Some(method));
        }
        if let Some(id) = default.as_str()
            && let Ok(method) = stripe.payment_method(id).await
            && let Some(method) = payment_method(&method)
        {
            return Ok(Some(method));
        }
    }
    let Some(customer) = customer else {
        return Ok(None);
    };
    let customer = match stripe.customer_with_default_payment_method(customer).await {
        Ok(customer) => customer,
        Err(StripeError::Missing(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    if customer["deleted"] == true {
        return Ok(None);
    }
    Ok(payment_method(
        &customer["invoice_settings"]["default_payment_method"],
    ))
}

async fn preview(
    stripe: &Stripe,
    company_id: i32,
    customer: Option<&str>,
    subscription_id: &str,
) -> Option<AccountNextInvoice> {
    match stripe.preview_invoice(customer?, subscription_id).await {
        Ok(preview) => Some(next_invoice(&preview)),
        // `invoice_upcoming_none`: the subscription ends, no next invoice.
        Err(StripeError::Missing(_)) => None,
        Err(err) => {
            tracing::warn!(
                company_id,
                error = err.code(),
                "account page: invoice preview failed"
            );
            None
        }
    }
}

/// Where the subscription stands: live from Stripe, else as stored.
struct Timeline {
    status: String,
    trialing: bool,
    ended: bool,
    period_start: Option<NaiveDateTime>,
    period_end: Option<NaiveDateTime>,
    trial_end: Option<NaiveDateTime>,
    next_charge_at: Option<NaiveDateTime>,
}

fn read_timeline(
    live: Option<&SubscriptionSnapshot>,
    record: Option<&CompanyBilling>,
    cancellation_scheduled: bool,
) -> Timeline {
    let status = live
        .map(|live| live.status.clone())
        .or_else(|| record.map(|r| r.subscription_status.clone()))
        .unwrap_or_else(|| "none".into());
    let trialing = status == "trialing";
    let ended = record.is_none() || is_ended_subscription_status(&status);
    let period_start = live
        .and_then(|live| live.period_start)
        .or_else(|| record.and_then(|r| r.current_period_start));
    let period_end = live
        .and_then(|live| live.period_end)
        .or_else(|| record.and_then(|r| r.current_period_end));
    let trial_end = live
        .and_then(|live| live.trial_end)
        .or_else(|| record.and_then(|r| r.trial_end_at));
    let ends_at = live.map_or_else(|| record.and_then(|r| r.cancel_at), |live| live.cancel_at);
    let next_charge_at = if cancellation_scheduled || ends_at.is_some() || ended {
        None
    } else if trialing {
        trial_end
    } else {
        period_end
    };
    Timeline {
        status,
        trialing,
        ended,
        period_start,
        period_end,
        trial_end,
        next_charge_at,
    }
}

/// Every add-on with its status, price and proration estimate.
fn addon_lines(
    live: Option<&SubscriptionSnapshot>,
    catalog: Option<&Catalog>,
    rows: &[(String, String)],
    timeline: &Timeline,
    now: NaiveDateTime,
) -> Vec<AccountAddon> {
    AddonKey::ALL
        .into_iter()
        .map(|key| {
            let on_subscription = live.map_or_else(
                || has_row(rows, key, "subscription"),
                |live| addon_item(live, key).is_some(),
            );
            let status = addon_status(has_row(rows, key, "comp"), on_subscription);
            let monthly_cents = live
                .and_then(|live| addon_item(live, key)?.unit_amount_cents)
                .or_else(|| catalog.map(|catalog| catalog.addon(key).unit_cents));
            let prorated = monthly_cents.map(|cents| {
                if status == AddonStatus::Included {
                    0
                } else {
                    prorated_cents(
                        cents,
                        now,
                        timeline.period_start,
                        timeline.period_end,
                        timeline.trialing,
                    )
                }
            });
            AccountAddon {
                key,
                label: key.label().into(),
                description: description(key).into(),
                monthly_cents,
                status,
                prorated_cents: prorated,
            }
        })
        .collect()
}

/// A live item's unit price, by lookup key.
fn live_unit_cents(live: Option<&SubscriptionSnapshot>, lookup_key: &str) -> Option<i64> {
    live.and_then(|live| item_with(live, lookup_key)?.unit_amount_cents)
}

/// The account page of `company_id`. Stripe failures fall back to the
/// stored rows; only database errors fail. Each Stripe read may take
/// [`PAGE_STRIPE_TIMEOUT`]. The subscription, prices and invoices are read
/// together, then the invoice preview and payment method, unless Stripe
/// could not be reached for the subscription.
pub async fn account_view(state: &AppState, company_id: i32) -> sqlx::Result<AccountSubscription> {
    let pool = &state.pool;
    let company = company(pool, company_id).await?;
    let company_name = company
        .as_ref()
        .and_then(|company| company.name.as_deref())
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if company
        .as_ref()
        .is_none_or(|company| company.plan_source != "subscription")
    {
        return Ok(AccountSubscription::legacy(company_name));
    }
    let now = super::now();
    let record = MySqlBillingStore(pool).by_company(company_id).await?;
    let stripe = page_stripe(&state.stripe);
    let (live, catalog, invoices) = tokio::join!(
        live_subscription(&stripe, company_id, record.as_ref()),
        Catalog::load(&stripe),
        account::page_invoices(&stripe, pool, record.as_ref()),
    );
    // Stripe cannot be reached: the reads that need the subscription would
    // only wait out their own timeouts, so they are skipped.
    let reachable = live.is_ok();
    let live = live.unwrap_or_default();
    let snapshot = live.as_ref().map(|(_, snapshot)| snapshot);
    let catalog = match catalog {
        Ok(catalog) => Some(catalog),
        Err(err) => {
            if stripe.is_enabled() {
                tracing::warn!(company_id, error = %err.reason(), "account page: prices unavailable");
            }
            None
        }
    };
    let invoices = invoices?
        .into_iter()
        .take(INVOICE_LIMIT)
        .map(Invoice::from)
        .collect();
    let billable = seats::billable_users(pool, company_id).await?;
    let rows = active_addons(pool, company_id).await?;
    let cancellation = cancellation::scheduled(pool, company_id)
        .await?
        .map(CancellationRequestBody::from);
    let timeline = read_timeline(snapshot, record.as_ref(), cancellation.is_some());
    let customer = record
        .as_ref()
        .and_then(|r| r.stripe_customer_id.as_deref());
    let (next_invoice, payment_method) = if reachable {
        tokio::join!(
            async {
                match snapshot {
                    Some(live) if !timeline.ended => {
                        preview(&stripe, company_id, customer, &live.id).await
                    }
                    _ => None,
                }
            },
            current_payment_method(&stripe, live.as_ref().map(|(raw, _)| raw), customer),
        )
    } else {
        (
            None,
            Err(StripeError::Request("Stripe could not be reached".into())),
        )
    };
    let payment_method_known = payment_method.is_ok();
    let payment_method = payment_method.unwrap_or_else(|err| {
        if stripe.is_enabled() && reachable {
            tracing::warn!(company_id, error = %err.code(), "account page: payment method unavailable");
        }
        None
    });

    let base_cents = live_unit_cents(snapshot, BASE_LOOKUP_KEY)
        .or_else(|| catalog.as_ref().map(|catalog| catalog.base.unit_cents));
    let extra_unit_cents = live_unit_cents(snapshot, EXTRA_USER_LOOKUP_KEY).or_else(|| {
        catalog
            .as_ref()
            .map(|catalog| catalog.extra_user.unit_cents)
    });
    let extra = seats::extra_users(billable);
    let addons = addon_lines(snapshot, catalog.as_ref(), &rows, &timeline, now);
    let has_subscription = record
        .as_ref()
        .is_some_and(|r| r.stripe_subscription_id.is_some());
    let can_cancel = has_subscription && is_running(&timeline.status);
    let can_change = stripe.is_enabled() && catalog.is_some() && can_cancel;

    Ok(AccountSubscription {
        company_name,
        plan_source: "subscription".into(),
        payment_status: record
            .as_ref()
            .map_or("none", |r| {
                with_grace_expiry(r, now).payment_status.as_str()
            })
            .into(),
        status: timeline.status,
        trial_end_at: utc(timeline.trial_end),
        current_period_end: utc(timeline.period_end),
        next_charge_at: utc(timeline.next_charge_at),
        base: base_cents.map(|monthly_cents| AccountBasePlan {
            label: "Base plan".into(),
            monthly_cents,
            included_users: INCLUDED_USERS,
        }),
        users: Some(AccountUsers {
            billable,
            included: INCLUDED_USERS,
            extra,
            extra_unit_cents,
            extra_monthly_cents: extra_unit_cents.map(|cents| cents * extra),
        }),
        addons,
        monthly_total_cents: snapshot
            .and_then(|live| live.monthly_total_cents)
            .or_else(|| record.as_ref().and_then(|r| r.monthly_price_cents)),
        next_invoice,
        payment_method,
        payment_method_known,
        cancellation,
        invoices,
        can_change,
        can_cancel,
    })
}

#[derive(Debug, sqlx::FromRow)]
struct PreviousAddon {
    stripe_subscription_item_id: Option<String>,
    started_at: NaiveDateTime,
    ended_at: Option<NaiveDateTime>,
}

async fn previous_addon(
    pool: &MySqlPool,
    company_id: i32,
    key: AddonKey,
) -> sqlx::Result<Option<PreviousAddon>> {
    sqlx::query_as(
        "SELECT stripe_subscription_item_id, started_at, ended_at
           FROM company_addons
          WHERE company_id = ? AND addon_key = ? AND source = 'subscription'",
    )
    .bind(company_id)
    .bind(key.as_str())
    .fetch_optional(pool)
    .await
}

/// The add-on bought through `item_id` from `now` (the webhook item sync
/// writes the same row).
async fn record_added(
    pool: &MySqlPool,
    company_id: i32,
    key: AddonKey,
    item_id: &str,
    now: NaiveDateTime,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO company_addons
           (company_id, addon_key, source, stripe_subscription_item_id, started_at, ended_at)
         VALUES (?, ?, 'subscription', ?, ?, NULL)
         ON DUPLICATE KEY UPDATE
           started_at = IF(ended_at IS NULL, started_at, VALUES(started_at)),
           stripe_subscription_item_id = VALUES(stripe_subscription_item_id),
           ended_at = NULL",
    )
    .bind(company_id)
    .bind(key.as_str())
    .bind(item_id)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

async fn record_removed(
    pool: &MySqlPool,
    company_id: i32,
    key: AddonKey,
    now: NaiveDateTime,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE company_addons SET ended_at = ?
          WHERE company_id = ? AND addon_key = ? AND source = 'subscription'
            AND ended_at IS NULL",
    )
    .bind(now)
    .bind(company_id)
    .bind(key.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

fn created_item_id(created: &Value) -> Result<String, AddonError> {
    created["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| StripeError::Request("Stripe did not return the new item".into()).into())
}

/// Adds the add-on's item (quantity 1, prorated); answers its id.
async fn add_item(
    state: &AppState,
    company_id: i32,
    key: AddonKey,
    subscription_id: &str,
    price_id: &str,
) -> Result<String, AddonError> {
    let previous = previous_addon(&state.pool, company_id, key).await?;
    let previous_item = previous
        .as_ref()
        .and_then(|row| row.stripe_subscription_item_id.as_deref());
    let idempotency_key = format!(
        "addon:{company_id}:{}:{}",
        key.as_str(),
        add_marker(previous.as_ref())
    );
    let stripe = &state.stripe;
    match stripe
        .create_subscription_item(subscription_id, price_id, 1, &idempotency_key)
        .await
    {
        Ok(created) => {
            let id = created_item_id(&created)?;
            if previous_item != Some(id.as_str()) {
                return Ok(id);
            }
        }
        Err(err) if is_key_reuse(&err) => {}
        Err(err) => return Err(err.into()),
    }
    // The key replayed an earlier add whose item is gone (or was used with
    // another price): once more under a key of its own.
    let fresh = format!(
        "addon:{company_id}:{}:{}",
        key.as_str(),
        uuid::Uuid::new_v4()
    );
    let created = stripe
        .create_subscription_item(subscription_id, price_id, 1, &fresh)
        .await?;
    created_item_id(&created)
}

/// Adds (`enabled`) or removes the add-on on the company's subscription,
/// under its billing lock. Already so: nothing is sent to Stripe.
pub async fn set_addon(
    state: &AppState,
    company_id: i32,
    key: AddonKey,
    enabled: bool,
) -> Result<(), AddonError> {
    let pool = &state.pool;
    if company(pool, company_id)
        .await?
        .is_none_or(|company| company.plan_source != "subscription")
    {
        return Err(refused(LEGACY_PLAN));
    }
    if has_row(&active_addons(pool, company_id).await?, key, "comp") {
        return Err(AddonError::Refused(format!(
            "{} is included free with your plan. Contact {SUPPORT_EMAIL} to change it.",
            key.label()
        )));
    }
    if !state.stripe.is_enabled() {
        return Err(StripeError::NotConfigured.into());
    }
    with_company_lock(
        pool,
        company_id,
        set_locked(state, company_id, key, enabled),
    )
    .await
}

async fn set_locked(
    state: &AppState,
    company_id: i32,
    key: AddonKey,
    enabled: bool,
) -> Result<(), AddonError> {
    let (pool, stripe) = (&state.pool, &state.stripe);
    let Some(record) = MySqlBillingStore(pool).by_company(company_id).await? else {
        return Err(refused(NO_SUBSCRIPTION));
    };
    let Some(subscription_id) = record.stripe_subscription_id.as_deref() else {
        return Err(refused(NO_SUBSCRIPTION));
    };
    let live = match stripe.subscription(subscription_id).await {
        Ok(raw) => subscription_snapshot(&raw),
        Err(StripeError::Missing(_)) => return Err(refused(NO_SUBSCRIPTION)),
        Err(err) => return Err(err.into()),
    };
    if !is_running(&live.status) {
        return Err(refused(NO_SUBSCRIPTION));
    }
    let now = super::now();
    match (enabled, addon_item(&live, key)) {
        (true, Some(item)) => record_added(pool, company_id, key, &item.id, now).await?,
        (false, None) => record_removed(pool, company_id, key, now).await?,
        (true, None) => {
            if live.status == "past_due" || is_overdue(&with_grace_expiry(&record, now)) {
                return Err(refused(PAY_FIRST));
            }
            let catalog = Catalog::load(stripe).await?;
            let price_id = &catalog.addon(key).price_id;
            let item_id = add_item(state, company_id, key, subscription_id, price_id).await?;
            record_added(pool, company_id, key, &item_id, now).await?;
        }
        (false, Some(item)) => {
            match stripe.delete_subscription_item(&item.id).await {
                Ok(_) | Err(StripeError::Missing(_)) => {}
                Err(err) => return Err(err.into()),
            }
            record_removed(pool, company_id, key, now).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::test_store::at;
    use serde_json::json;

    #[test]
    fn prorates_by_the_seconds_left() {
        let (start, end) = (at("2027-01-01 00:00:00"), at("2027-01-31 00:00:00"));
        let prorate = |now: &str| prorated_cents(7_500, at(now), Some(start), Some(end), false);
        assert_eq!(prorate("2027-01-16 00:00:00"), 3_750);
        assert_eq!(prorate("2027-01-21 00:00:00"), 2_500);
        assert_eq!(prorate("2027-01-01 00:00:00"), 7_500);
        // Before the period: the whole period is left; after it: nothing.
        assert_eq!(prorate("2026-12-25 00:00:00"), 7_500);
        assert_eq!(prorate("2027-01-31 00:00:00"), 0);
        assert_eq!(prorate("2027-02-05 00:00:00"), 0);
    }

    #[test]
    fn prorations_round_to_the_nearest_cent() {
        let start = at("2027-01-01 00:00:00");
        let end = at("2027-01-01 00:00:03");
        assert_eq!(
            prorated_cents(
                1_000,
                at("2027-01-01 00:00:02"),
                Some(start),
                Some(end),
                false
            ),
            333
        );
        assert_eq!(
            prorated_cents(
                1_000,
                at("2027-01-01 00:00:01"),
                Some(start),
                Some(end),
                false
            ),
            667
        );
    }

    #[test]
    fn nothing_is_prorated_while_trialing_or_without_a_period() {
        let (start, end) = (at("2027-01-01 00:00:00"), at("2027-01-31 00:00:00"));
        let now = at("2027-01-16 00:00:00");
        assert_eq!(prorated_cents(7_500, now, Some(start), Some(end), true), 0);
        assert_eq!(prorated_cents(7_500, now, None, Some(end), false), 0);
        assert_eq!(prorated_cents(7_500, now, Some(start), None, false), 0);
        assert_eq!(prorated_cents(7_500, now, Some(end), Some(end), false), 0);
        assert_eq!(prorated_cents(7_500, now, Some(end), Some(start), false), 0);
        assert_eq!(prorated_cents(0, now, Some(start), Some(end), false), 0);
    }

    #[test]
    fn comp_wins_over_the_subscription() {
        assert_eq!(addon_status(true, true), AddonStatus::Included);
        assert_eq!(addon_status(true, false), AddonStatus::Included);
        assert_eq!(addon_status(false, true), AddonStatus::Active);
        assert_eq!(addon_status(false, false), AddonStatus::Available);
    }

    #[test]
    fn add_markers_change_between_adds() {
        assert_eq!(add_marker(None), "first");
        let ended = PreviousAddon {
            stripe_subscription_item_id: Some("si_1".into()),
            started_at: at("2026-10-01 00:00:00"),
            ended_at: Some(at("2026-10-02 00:00:00")),
        };
        assert_eq!(add_marker(Some(&ended)), "1790899200");
        let stale = PreviousAddon {
            ended_at: None,
            ..ended
        };
        assert_eq!(add_marker(Some(&stale)), "s1790812800");
    }

    #[test]
    fn previews_map_tax_lines_and_date() {
        let preview = json!({
            "subtotal": 43_500, "total": 46_000, "amount_due": 45_000,
            "total_taxes": [{ "amount": 2_000 }, { "amount": 500 }],
            "next_payment_attempt": 1_800_000_000, "period_end": 1_700_000_000,
            "lines": { "data": [
                { "description": "Quotes (Jan 21 to Feb 21)", "amount": 7_500 },
                { "description": "Unused time on Signatures", "amount": -500 },
            ] },
        });
        let next = next_invoice(&preview);
        assert_eq!(
            (
                next.subtotal_cents,
                next.tax_cents,
                next.total_cents,
                next.amount_due_cents
            ),
            (43_500, 2_500, 46_000, 45_000)
        );
        assert_eq!(next.date, DateTime::from_timestamp(1_800_000_000, 0));
        assert_eq!(next.lines[1].amount_cents, -500);
        assert_eq!(next.lines[0].description, "Quotes (Jan 21 to Feb 21)");

        let bare = next_invoice(&json!({ "subtotal": 100, "total": 100, "amount_due": 100,
            "period_end": 1_700_000_000 }));
        assert_eq!(bare.tax_cents, 0);
        assert_eq!(bare.date, DateTime::from_timestamp(1_700_000_000, 0));
        assert!(bare.lines.is_empty());
    }

    #[test]
    fn payment_methods_keep_only_what_the_page_shows() {
        let card = payment_method(&json!({ "id": "pm_1", "type": "card",
            "card": { "brand": "visa", "last4": "4242", "exp_month": 12, "exp_year": 2030 } }))
        .unwrap();
        assert_eq!(card.kind, "card");
        assert_eq!(
            (
                card.brand.as_deref(),
                card.last4.as_deref(),
                card.exp_month,
                card.exp_year
            ),
            (Some("visa"), Some("4242"), Some(12), Some(2030))
        );
        assert_eq!(card.bank_name, None);
        let bank = payment_method(&json!({ "id": "pm_2", "type": "us_bank_account",
            "us_bank_account": { "bank_name": "STRIPE TEST BANK", "last4": "6789" } }))
        .unwrap();
        assert_eq!(
            (
                bank.kind.as_str(),
                bank.bank_name.as_deref(),
                bank.last4.as_deref(),
                bank.brand
            ),
            (
                "us_bank_account",
                Some("STRIPE TEST BANK"),
                Some("6789"),
                None
            )
        );
        assert_eq!(payment_method(&json!("pm_3")), None);
        assert_eq!(payment_method(&Value::Null), None);
    }
}
