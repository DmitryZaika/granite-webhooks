//! The billing page's Stripe calls.
//!
//! The company's record refreshed from Stripe, its card and invoices, and
//! starting the billing portal or a plan change. Port of
//! `billingSync.server.ts`, `billingCheckout.server.ts` and
//! `billingPageData.server.ts`.
#![allow(clippy::future_not_send)]

use super::machine::apply_subscription_change;
use super::reactivation::{
    CompanyLocking, NOT_CONFIGURED, ReactivationError, locked, refresh_subscription,
};
use super::store::{BillingStore, MySqlBillingStore};
use super::stripe::{Stripe, invoice_snapshot, subscription_snapshot};
use super::{
    BillingInvoice, BillingPlan, CompanyBilling, is_ended_subscription_status,
    outstanding_balance_cents,
};
use chrono::{DateTime, NaiveDateTime};
use lambda_http::tracing;
use serde_json::Value;
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Flat price per company: every subscription is one unit of the plan.
pub const FLAT_QUANTITY: i64 = 1;
/// How often a billing page load reads Stripe for one company.
const PAGE_SYNC_INTERVAL: Duration = Duration::from_mins(5);
const PORTAL_CONFIGURATION_TAG: &str = "granite-manager-no-cancel-v1";
const PORTAL_CONFIGURATION_NAME: &str = "Granite Manager (no self-cancel)";

/// When each company was last synced, per Stripe base (per Lambda container,
/// as the CRM kept it per server process).
static LAST_PAGE_SYNC: LazyLock<Mutex<HashMap<(String, i32), Instant>>> =
    LazyLock::new(Mutex::default);
/// The no-cancel portal configuration id, per Stripe base.
static PORTAL_CONFIGURATIONS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(Mutex::default);

fn refused(message: &str) -> ReactivationError {
    ReactivationError::Refused(message.into())
}

/// How [`sync_from_stripe`] runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncOptions {
    /// Read Stripe even when it was read in the last 5 minutes (back from
    /// Checkout, reconciliation).
    pub force: bool,
    /// Fail instead of falling back to the stored row.
    pub throw_on_error: bool,
}

/// The record with `last_successful_payment_at` moved forward to the latest
/// paid invoice, never back.
///
/// Only the `invoice.paid` webhook sets it, so a lost webhook would leave it
/// behind.
pub fn with_latest_payment(record: CompanyBilling, invoices: &[BillingInvoice]) -> CompanyBilling {
    let latest = invoices
        .iter()
        .filter(|invoice| invoice.status == "paid")
        .filter_map(|invoice| invoice.paid_at)
        .fold(record.last_successful_payment_at, |latest, paid_at| {
            Some(latest.map_or(paid_at, |latest| latest.max(paid_at)))
        });
    CompanyBilling {
        last_successful_payment_at: latest,
        ..record
    }
}

fn sync_due(last: Option<Instant>, now: Instant, force: bool) -> bool {
    force || last.is_none_or(|last| now.duration_since(last) >= PAGE_SYNC_INTERVAL)
}

/// The company's record after pulling its subscription and latest invoices
/// from Stripe, under the company's lock.
///
/// Page loads read Stripe at most every 5 minutes per company unless
/// `force`. A failure (Stripe, a busy lock) keeps the stored row, or is
/// returned with `throw_on_error`.
pub async fn sync_from_stripe(
    stripe: &Stripe,
    pool: &MySqlPool,
    company_id: i32,
    options: SyncOptions,
) -> Result<Option<CompanyBilling>, ReactivationError> {
    let store = MySqlBillingStore(pool);
    let Some(record) = store.by_company(company_id).await? else {
        return Ok(None);
    };
    if !stripe.is_enabled() {
        return Ok(Some(record));
    }
    let now = Instant::now();
    {
        let Ok(mut last) = LAST_PAGE_SYNC.lock() else {
            return Ok(Some(record));
        };
        let key = (stripe.base_url().to_string(), company_id);
        if !sync_due(last.get(&key).copied(), now, options.force) {
            return Ok(Some(record));
        }
        last.insert(key, now);
    }
    let synced = locked(
        &store,
        company_id,
        sync_locked(stripe, &store, record.clone()),
    )
    .await;
    match synced {
        Ok(()) => Ok(store.by_company(company_id).await?),
        Err(error) if options.throw_on_error => Err(error),
        Err(error) => {
            tracing::warn!(%error, company_id, "billing sync from Stripe failed");
            Ok(store.by_company(company_id).await?.or(Some(record)))
        }
    }
}

async fn sync_locked(
    stripe: &Stripe,
    store: &MySqlBillingStore<'_>,
    record: CompanyBilling,
) -> Result<(), ReactivationError> {
    let company_id = record.company_id;
    let customer = record.stripe_customer_id.clone();
    if record.stripe_subscription_id.is_some() || customer.is_some() {
        refresh_subscription(stripe, store, record, super::now()).await?;
    }
    let Some(customer) = customer else {
        return Ok(());
    };
    for invoice in stripe.list_invoices(&customer).await? {
        let snapshot = invoice_snapshot(&invoice);
        if snapshot.id.is_empty() || !matches!(snapshot.status.as_str(), "paid" | "open") {
            continue;
        }
        let paid = snapshot.status == "paid";
        store
            .upsert_invoice(&BillingInvoice {
                company_id,
                stripe_invoice_id: snapshot.id,
                amount_due_cents: if paid { 0 } else { snapshot.amount_due_cents },
                amount_paid_cents: snapshot.amount_paid_cents,
                currency: snapshot.currency,
                hosted_invoice_url: snapshot.hosted_invoice_url,
                invoice_pdf: snapshot.invoice_pdf,
                paid_at: if paid { snapshot.paid_at } else { None },
                status: snapshot.status,
            })
            .await?;
    }
    if let Some(current) = store.by_company(company_id).await? {
        let stored = store.invoices(company_id).await?;
        store
            .save(&CompanyBilling {
                outstanding_balance_cents: outstanding_balance_cents(&stored),
                ..with_latest_payment(current, &stored)
            })
            .await?;
    }
    Ok(())
}

/// What a reconciliation pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reconciled {
    pub checked: usize,
    /// Companies Stripe could not be read for (or whose lock was busy).
    pub failed: usize,
}

/// One batch of a reconciliation pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileBatch {
    pub reconciled: Reconciled,
    /// The last company id synced; the next batch starts after it.
    pub last_id: Option<i32>,
    /// No company is left after `last_id`: the pass is over.
    pub done: bool,
}

/// Re-reads every Stripe-billed company from Stripe (forced).
///
/// A webhook Stripe gave up on would otherwise leave a company paid after a
/// failed renewal, or paused after a payment. One company's failure does not
/// stop the rest. `None` without Stripe. Throttling is the caller's.
pub async fn reconcile_with_stripe(
    stripe: &Stripe,
    pool: &MySqlPool,
) -> Result<Option<Reconciled>, sqlx::Error> {
    Ok(reconcile_batch(stripe, pool, 0, 500, None)
        .await?
        .map(|batch| batch.reconciled))
}

/// The Stripe-billed companies after id `after`, at most `limit`, stopping
/// early once `budget` has passed (one pass split over several job runs).
pub async fn reconcile_batch(
    stripe: &Stripe,
    pool: &MySqlPool,
    after: i32,
    limit: u32,
    budget: Option<Duration>,
) -> Result<Option<ReconcileBatch>, sqlx::Error> {
    if !stripe.is_enabled() {
        return Ok(None);
    }
    let started = Instant::now();
    let company_ids: Vec<i32> = sqlx::query_scalar(
        "SELECT c.id
           FROM company c
           JOIN company_billing cb ON cb.company_id = c.id
          WHERE c.billing_required = 1 AND c.deleted_at IS NULL
            AND cb.stripe_customer_id IS NOT NULL AND c.id > ?
          ORDER BY c.id
          LIMIT ?",
    )
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let options = SyncOptions {
        force: true,
        throw_on_error: true,
    };
    let (mut checked, mut failed, mut last_id) = (0, 0, None);
    for &company_id in &company_ids {
        if budget.is_some_and(|budget| started.elapsed() >= budget) {
            break;
        }
        if let Err(error) = sync_from_stripe(stripe, pool, company_id, options).await {
            tracing::warn!(%error, company_id, "billing reconciliation failed");
            failed += 1;
        } else {
            checked += 1;
        }
        last_id = Some(company_id);
    }
    let finished_all = checked + failed == company_ids.len();
    Ok(Some(ReconcileBatch {
        reconciled: Reconciled { checked, failed },
        last_id,
        done: finished_all && company_ids.len() < usize::try_from(limit).unwrap_or(usize::MAX),
    }))
}

/// A saved card.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct Card {
    /// Stripe payment method id (`pm_...`).
    pub id: String,
    /// `visa`, `mastercard`, ...
    pub brand: String,
    /// Last four digits of the card number.
    pub last4: String,
    /// Expiry month (1-12).
    pub exp_month: i64,
    /// Expiry year (e.g. 2030).
    pub exp_year: i64,
}

fn card(method: &Value) -> Option<Card> {
    let card = method.get("card").filter(|card| card.is_object())?;
    Some(Card {
        id: method["id"].as_str()?.to_string(),
        brand: card["brand"].as_str().unwrap_or_default().to_string(),
        last4: card["last4"].as_str().unwrap_or_default().to_string(),
        exp_month: card["exp_month"].as_i64().unwrap_or_default(),
        exp_year: card["exp_year"].as_i64().unwrap_or_default(),
    })
}

/// The card renewals are charged to: the subscription's own default, else
/// the customer's (the portal's card form may set only that one). `None`
/// when Stripe cannot say.
pub async fn default_card(stripe: &Stripe, record: Option<&CompanyBilling>) -> Option<Card> {
    let record = record?;
    if !stripe.is_enabled() {
        return None;
    }
    if let Some(id) = &record.stripe_payment_method_id
        && let Some(card) = card(&stripe.payment_method(id).await.ok()?)
    {
        return Some(card);
    }
    let customer = stripe
        .customer_with_default_payment_method(record.stripe_customer_id.as_deref()?)
        .await
        .ok()?;
    if customer["deleted"] == true {
        return None;
    }
    card(&customer["invoice_settings"]["default_payment_method"])
}

/// The customer's saved cards, the default one first.
pub async fn saved_cards(stripe: &Stripe, record: Option<&CompanyBilling>) -> Vec<Card> {
    let Some(record) = record else {
        return Vec::new();
    };
    let Some(customer) = &record.stripe_customer_id else {
        return Vec::new();
    };
    if !stripe.is_enabled() {
        return Vec::new();
    }
    let mut cards: Vec<Card> = stripe
        .list_cards(customer)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(card)
        .collect();
    let preferred = record.stripe_payment_method_id.as_deref();
    cards.sort_by_key(|card| Some(card.id.as_str()) != preferred);
    cards
}

/// An invoice as the billing page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageInvoice {
    pub stripe_invoice_id: String,
    /// Stripe's invoice number; `None` from the stored copy.
    pub number: Option<String>,
    pub status: String,
    pub amount_due_cents: i64,
    pub amount_paid_cents: i64,
    pub hosted_invoice_url: Option<String>,
    pub invoice_pdf: Option<String>,
    pub paid_at: Option<NaiveDateTime>,
    /// When Stripe issued it; `None` from the stored copy.
    pub created_at: Option<NaiveDateTime>,
}

impl From<BillingInvoice> for PageInvoice {
    fn from(invoice: BillingInvoice) -> Self {
        Self {
            stripe_invoice_id: invoice.stripe_invoice_id,
            number: None,
            status: invoice.status,
            amount_due_cents: invoice.amount_due_cents,
            amount_paid_cents: invoice.amount_paid_cents,
            hosted_invoice_url: invoice.hosted_invoice_url,
            invoice_pdf: invoice.invoice_pdf,
            paid_at: invoice.paid_at,
            created_at: None,
        }
    }
}

fn unix_date(value: &Value) -> Option<NaiveDateTime> {
    DateTime::from_timestamp(value.as_i64()?, 0).map(|date| date.naive_utc())
}

/// A Stripe invoice for the page; drafts are not shown.
fn live_invoice(invoice: &Value) -> Option<PageInvoice> {
    let status = invoice["status"]
        .as_str()
        .filter(|status| !status.is_empty() && *status != "draft")?;
    let paid = status == "paid";
    Some(PageInvoice {
        stripe_invoice_id: invoice["id"].as_str().unwrap_or_default().to_string(),
        number: invoice["number"].as_str().map(str::to_string),
        status: status.to_string(),
        amount_due_cents: if paid {
            0
        } else {
            invoice["amount_due"].as_i64().unwrap_or_default()
        },
        amount_paid_cents: invoice["amount_paid"].as_i64().unwrap_or_default(),
        hosted_invoice_url: invoice["hosted_invoice_url"].as_str().map(str::to_string),
        invoice_pdf: invoice["invoice_pdf"].as_str().map(str::to_string),
        paid_at: if paid {
            unix_date(&invoice["status_transitions"]["paid_at"])
        } else {
            None
        },
        created_at: unix_date(&invoice["created"]),
    })
}

/// The invoices to list, newest first: from Stripe (with number and issue
/// date), else the stored copy when Stripe cannot be reached.
pub async fn page_invoices(
    stripe: &Stripe,
    pool: &MySqlPool,
    record: Option<&CompanyBilling>,
) -> Result<Vec<PageInvoice>, sqlx::Error> {
    let Some(record) = record else {
        return Ok(Vec::new());
    };
    if stripe.is_enabled()
        && let Some(customer) = &record.stripe_customer_id
    {
        match stripe.list_invoices(customer).await {
            Ok(invoices) => return Ok(invoices.iter().filter_map(live_invoice).collect()),
            Err(error) => tracing::warn!(%error, "billing invoices from Stripe failed"),
        }
    }
    let stored = MySqlBillingStore(pool).invoices(record.company_id).await?;
    Ok(stored.into_iter().map(PageInvoice::from).collect())
}

fn check_plan(plans: &[BillingPlan], price_id: &str) -> Result<(), ReactivationError> {
    if plans.iter().any(|plan| plan.price_id == price_id) {
        Ok(())
    } else {
        Err(refused("Unknown billing plan"))
    }
}

/// The active portal configuration with the no-cancel tag.
fn tagged_configuration(configurations: &[Value]) -> Option<String> {
    configurations
        .iter()
        .find(|configuration| configuration["metadata"]["app"] == PORTAL_CONFIGURATION_TAG)
        .and_then(|configuration| configuration["id"].as_str())
        .map(str::to_string)
}

/// A portal setup where companies manage cards, details and invoices but
/// cannot cancel or change the plan (that goes through a manager or
/// support). Found by its tag or created once, so no Dashboard setup is needed.
async fn portal_configuration(stripe: &Stripe) -> Result<String, ReactivationError> {
    let base = stripe.base_url().to_string();
    let cached = PORTAL_CONFIGURATIONS
        .lock()
        .ok()
        .and_then(|ids| ids.get(&base).cloned());
    if let Some(id) = cached {
        return Ok(id);
    }
    let id = match tagged_configuration(&stripe.list_portal_configurations().await?) {
        Some(id) => id,
        None => stripe
            .create_portal_configuration(PORTAL_CONFIGURATION_NAME, PORTAL_CONFIGURATION_TAG)
            .await?["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| refused("Stripe did not return a billing link"))?,
    };
    if let Ok(mut ids) = PORTAL_CONFIGURATIONS.lock() {
        ids.insert(base, id.clone());
    }
    Ok(id)
}

/// A billing portal link (no cancelling): the card form, or the whole
/// subscription.
pub async fn portal_url<S: BillingStore>(
    stripe: &Stripe,
    store: &S,
    company_id: i32,
    billing_url: &str,
    payment_method_update: bool,
) -> Result<String, ReactivationError> {
    if !stripe.is_enabled() {
        return Err(refused(NOT_CONFIGURED));
    }
    let customer = store
        .by_company(company_id)
        .await?
        .and_then(|record| record.stripe_customer_id)
        .ok_or_else(|| refused("Subscribe to a plan first."))?;
    let configuration = portal_configuration(stripe).await?;
    let session = stripe
        .create_portal_session(
            &customer,
            &configuration,
            billing_url,
            payment_method_update,
        )
        .await?;
    session["url"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| refused("Stripe did not return a billing link"))
}

/// Moves the running subscription to another plan, under the company's lock.
pub async fn update_plan<S: BillingStore + CompanyLocking>(
    stripe: &Stripe,
    store: &S,
    plans: &[BillingPlan],
    company_id: i32,
    price_id: &str,
) -> Result<(), ReactivationError> {
    check_plan(plans, price_id)?;
    if !stripe.is_enabled() {
        return Err(refused(NOT_CONFIGURED));
    }
    locked(store, company_id, async {
        let record = store.by_company(company_id).await?;
        let Some((subscription_id, item_id)) = record
            .filter(|record| !is_ended_subscription_status(&record.subscription_status))
            .and_then(|record| {
                Some((
                    record.stripe_subscription_id?,
                    record.stripe_subscription_item_id?,
                ))
            })
        else {
            return Err(refused("No active subscription to update"));
        };
        let subscription = stripe
            .update_subscription(
                &subscription_id,
                &item_id,
                price_id,
                FLAT_QUANTITY,
                company_id,
            )
            .await?;
        apply_subscription_change(store, &subscription_snapshot(&subscription), super::now())
            .await?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::test_store::at;
    use serde_json::json;

    /// Port of `billingEvents.test.ts`'s `invoice` fixture.
    fn invoice(paid_at: Option<&str>, status: &str) -> BillingInvoice {
        BillingInvoice {
            company_id: 1,
            stripe_invoice_id: format!("in_{}", paid_at.unwrap_or("null")),
            status: status.into(),
            amount_due_cents: 0,
            amount_paid_cents: 30000,
            currency: "usd".into(),
            hosted_invoice_url: None,
            invoice_pdf: None,
            paid_at: paid_at.map(at),
        }
    }

    #[test]
    fn takes_the_newest_paid_invoice_as_the_last_payment() {
        let updated = with_latest_payment(
            CompanyBilling::empty(1),
            &[
                invoice(Some("2026-09-07 10:00:00"), "paid"),
                invoice(Some("2026-10-07 10:00:00"), "paid"),
                invoice(None, "open"),
            ],
        );
        assert_eq!(
            updated.last_successful_payment_at,
            Some(at("2026-10-07 10:00:00"))
        );
    }

    #[test]
    fn never_moves_an_already_recorded_payment_back() {
        let record = CompanyBilling {
            last_successful_payment_at: Some(at("2026-10-07 10:00:00")),
            ..CompanyBilling::empty(1)
        };
        assert_eq!(
            with_latest_payment(
                record.clone(),
                &[invoice(Some("2026-09-07 10:00:00"), "paid")]
            ),
            record
        );
        assert_eq!(with_latest_payment(record.clone(), &[]), record);
    }

    #[test]
    fn page_loads_read_stripe_every_five_minutes_unless_forced() {
        let start = Instant::now();
        assert!(sync_due(None, start, false));
        let soon = start + Duration::from_secs(60);
        assert!(!sync_due(Some(start), soon, false));
        assert!(sync_due(Some(start), soon, true));
        assert!(sync_due(Some(start), start + PAGE_SYNC_INTERVAL, false));
    }

    #[test]
    fn the_portal_uses_the_tagged_configuration() {
        assert_eq!(tagged_configuration(&[]), None);
        assert_eq!(
            tagged_configuration(&[
                json!({ "id": "bpc_default", "metadata": {} }),
                json!({ "id": "bpc_tagged", "metadata": { "app": PORTAL_CONFIGURATION_TAG } }),
            ]),
            Some("bpc_tagged".into())
        );
    }

    #[test]
    fn stripe_invoices_carry_number_and_issue_date_and_drafts_are_hidden() {
        let invoice = live_invoice(&json!({
            "id": "in_1", "number": "GM-0001", "status": "paid", "created": 1_788_000_000,
            "amount_due": 4900, "amount_paid": 4900, "hosted_invoice_url": "https://pay.stripe.com/i",
            "invoice_pdf": null, "status_transitions": { "paid_at": 1_788_220_800 },
        }))
        .unwrap();
        assert_eq!(invoice.number.as_deref(), Some("GM-0001"));
        assert_eq!(invoice.amount_due_cents, 0);
        assert_eq!(invoice.paid_at, Some(at("2026-09-01 00:00:00")));
        assert_eq!(invoice.created_at, Some(at("2026-08-29 10:40:00")));
        let open = live_invoice(&json!({ "id": "in_2", "status": "open", "amount_due": 4900,
            "status_transitions": { "paid_at": 1_788_220_800 } }))
        .unwrap();
        assert_eq!(
            (open.amount_due_cents, open.paid_at, open.number),
            (4900, None, None)
        );
        assert_eq!(
            live_invoice(&json!({ "id": "in_3", "status": "draft" })),
            None
        );
        assert_eq!(live_invoice(&json!({ "id": "in_4" })), None);
    }
}
