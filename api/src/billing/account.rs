//! The billing page's Stripe calls.
//!
//! The company's record refreshed from Stripe, its card, and starting
//! Checkout, the billing portal or a plan change. Port of `syncBillingFromStripe`, `createCheckoutUrl`, `createPortalUrl`,
//! `updateSubscriptionPlan` and `listSavedCards` in `stripeBilling.server.ts`.
#![allow(clippy::future_not_send)]

use super::machine::apply_subscription_change;
use super::reactivation::{ReactivationError, refresh_subscription};
use super::store::{BillingStore, MySqlBillingStore};
use super::stripe::{Stripe, invoice_snapshot, subscription_snapshot};
use super::{
    BillingInvoice, BillingPlan, CompanyBilling, can_start_checkout, is_ended_subscription_status,
    outstanding_balance_cents,
};
use lambda_http::tracing;
use serde_json::Value;
use sqlx::MySqlPool;

/// Flat price per company: every subscription is one unit of the plan.
pub const FLAT_QUANTITY: i64 = 1;

fn refused(message: &str) -> ReactivationError {
    ReactivationError::Refused(message.into())
}

/// The company's record after asking Stripe for its subscription and
/// latest invoices (Stripe failures keep what is stored).
pub async fn sync_from_stripe(
    stripe: &Stripe,
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Option<CompanyBilling>, sqlx::Error> {
    let store = MySqlBillingStore(pool);
    let Some(record) = store.by_company(company_id).await? else {
        return Ok(None);
    };
    if !stripe.is_enabled() {
        return Ok(Some(record));
    }
    let synced = async {
        if record.stripe_subscription_id.is_some() || record.stripe_customer_id.is_some() {
            refresh_subscription(stripe, &store, record.clone(), super::now()).await?;
        }
        if let Some(customer) = &record.stripe_customer_id {
            for invoice in stripe.list_invoices(customer).await? {
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
            if let Some(mut current) = store.by_company(company_id).await? {
                current.outstanding_balance_cents =
                    outstanding_balance_cents(&store.invoices(company_id).await?);
                store.save(&current).await?;
            }
        }
        Ok::<_, ReactivationError>(())
    }
    .await;
    if let Err(error) = synced {
        tracing::warn!(%error, company_id, "billing sync from Stripe failed");
    }
    store.by_company(company_id).await
}

/// A saved card.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, crate::openapi::ToSchema)]
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

/// The card subscriptions are charged to, if Stripe knows it.
pub async fn default_card(stripe: &Stripe, payment_method_id: Option<&str>) -> Option<Card> {
    let id = payment_method_id?;
    if !stripe.is_enabled() {
        return None;
    }
    card(&stripe.payment_method(id).await.ok()?)
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

fn check_plan(plans: &[BillingPlan], price_id: &str) -> Result<(), ReactivationError> {
    if plans.iter().any(|plan| plan.price_id == price_id) {
        Ok(())
    } else {
        Err(refused("Unknown billing plan"))
    }
}

/// The company's record with a Stripe customer, created on first use.
async fn ensure_customer<S: BillingStore>(
    stripe: &Stripe,
    store: &S,
    company_id: i32,
    company_name: &str,
) -> Result<CompanyBilling, ReactivationError> {
    let existing = store.by_company(company_id).await?;
    if let Some(record) = existing.as_ref().filter(|r| r.stripe_customer_id.is_some()) {
        return Ok(record.clone());
    }
    let customer = stripe.create_customer(company_name, company_id).await?;
    let id = customer["id"]
        .as_str()
        .ok_or_else(|| refused("Billing is not configured"))?;
    let record = CompanyBilling {
        stripe_customer_id: Some(id.to_string()),
        ..existing.unwrap_or_else(|| CompanyBilling::empty(company_id))
    };
    store.save(&record).await?;
    Ok(record)
}

/// A Checkout link for a company without a subscription.
pub async fn checkout_url<S: BillingStore>(
    stripe: &Stripe,
    store: &S,
    plans: &[BillingPlan],
    company_id: i32,
    company_name: &str,
    price_id: &str,
    billing_url: &str,
) -> Result<String, ReactivationError> {
    check_plan(plans, price_id)?;
    if !stripe.is_enabled() {
        return Err(refused("Billing is not configured"));
    }
    let record = ensure_customer(stripe, store, company_id, company_name).await?;
    let customer = record
        .stripe_customer_id
        .as_deref()
        .ok_or_else(|| refused("Billing is not configured"))?;
    if !can_start_checkout(Some(&record)) {
        return Err(refused(
            "This company already has a subscription. Update the plan instead.",
        ));
    }
    let session = stripe
        .create_checkout_session(customer, company_id, price_id, FLAT_QUANTITY, billing_url)
        .await?;
    session["url"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| refused("Stripe did not return a checkout link"))
}

/// A billing portal link: the card form, or the whole subscription.
pub async fn portal_url<S: BillingStore>(
    stripe: &Stripe,
    store: &S,
    company_id: i32,
    billing_url: &str,
    payment_method_update: bool,
) -> Result<String, ReactivationError> {
    if !stripe.is_enabled() {
        return Err(refused("Billing is not configured"));
    }
    let customer = store
        .by_company(company_id)
        .await?
        .and_then(|record| record.stripe_customer_id)
        .ok_or_else(|| refused("No Stripe customer for this company"))?;
    let session = stripe
        .create_portal_session(&customer, billing_url, payment_method_update)
        .await?;
    session["url"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| refused("Stripe did not return a billing link"))
}

/// Moves the running subscription to another plan.
pub async fn update_plan<S: BillingStore>(
    stripe: &Stripe,
    store: &S,
    plans: &[BillingPlan],
    company_id: i32,
    price_id: &str,
) -> Result<(), ReactivationError> {
    check_plan(plans, price_id)?;
    if !stripe.is_enabled() {
        return Err(refused("Billing is not configured"));
    }
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
    apply_subscription_change(store, &subscription_snapshot(&subscription), super::now()).await?;
    Ok(())
}
