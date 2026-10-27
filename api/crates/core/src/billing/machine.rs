//! The billing state machine: Stripe events in, `company_billing` and the
//! managers' notifications out. The access gate is `gate`.

use super::store::{BillingStore, StoreResult};
use super::{
    BILLING_PATH, BillingEvent, BillingInvoice, BillingNotification, CompanyBilling,
    InvoiceSnapshot, PAYMENT_REQUIRED_TITLE, PaymentStatus, SubscriptionSnapshot,
    UPDATE_PAYMENT_LABEL, add_grace_days, has_live_other_subscription,
    is_ended_subscription_status, is_overdue, is_paying_subscription_status, iso,
    outstanding_balance_cents, payment_required_message, with_grace_expiry,
};
use chrono::NaiveDateTime;

/// What applying an event did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyResult {
    /// The event was processed before; nothing changed.
    pub duplicate: bool,
    pub company_id: Option<i32>,
    /// Managers who got a new "payment required" notification.
    pub notified_user_ids: Vec<i32>,
}

const fn total_monthly(
    unit: Option<i64>,
    quantity: Option<i64>,
    previous: Option<i64>,
) -> Option<i64> {
    match (unit, quantity) {
        (Some(unit), Some(quantity)) => Some(unit * quantity),
        _ => previous,
    }
}

fn merge_subscription(record: &CompanyBilling, snapshot: &SubscriptionSnapshot) -> CompanyBilling {
    let mut merged = record.clone();
    if !snapshot.customer_id.is_empty() {
        merged.stripe_customer_id = Some(snapshot.customer_id.clone());
    }
    if !snapshot.id.is_empty() {
        merged.stripe_subscription_id = Some(snapshot.id.clone());
    }
    merged.stripe_price_id = snapshot.price_id.clone().or(merged.stripe_price_id);
    merged.stripe_subscription_item_id = snapshot
        .item_id
        .clone()
        .or(merged.stripe_subscription_item_id);
    merged.stripe_payment_method_id = snapshot
        .payment_method_id
        .clone()
        .or(merged.stripe_payment_method_id);
    merged.subscription_status = if snapshot.deleted {
        "canceled".into()
    } else {
        snapshot.status.clone()
    };
    merged.current_period_start = snapshot.period_start.or(merged.current_period_start);
    merged.current_period_end = snapshot.period_end.or(merged.current_period_end);
    // Mirrored as is: a cleared trial or a withdrawn cancellation clears them.
    merged.trial_end_at = snapshot.trial_end;
    merged.cancel_at = snapshot.cancel_at;
    merged.plan_name = snapshot.plan_name.clone().or(merged.plan_name);
    if let Some(total) = snapshot.monthly_total_cents {
        merged.monthly_price_cents = Some(total);
    } else {
        merged.user_count = snapshot.quantity.or(merged.user_count);
        merged.monthly_price_cents = total_monthly(
            snapshot.unit_amount_cents,
            snapshot.quantity,
            record.monthly_price_cents,
        );
    }
    merged
}

/// The record after a failed payment, and when to notify (a new failure only).
fn mark_past_due(
    record: &CompanyBilling,
    now: NaiveDateTime,
) -> (CompanyBilling, Option<NaiveDateTime>) {
    let mut next = record.clone();
    next.subscription_status = "past_due".into();
    if record.payment_status == PaymentStatus::Suspended {
        return (next, None);
    }
    if record.payment_status == PaymentStatus::PastDue
        && record.grace_period_ends_at.is_some()
        && record.failed_payment_at.is_some()
    {
        return (with_grace_expiry(&next, now), None);
    }
    next.payment_status = PaymentStatus::PastDue;
    next.failed_payment_at = Some(now);
    next.grace_period_ends_at = Some(add_grace_days(now));
    next.recovered_failed_at = None;
    next.recovered_at = None;
    (next, Some(now))
}

/// The record paid up; an overdue one records which cycle recovered, and when.
pub(super) fn restore_active(
    record: &CompanyBilling,
    paid_at: Option<NaiveDateTime>,
    now: NaiveDateTime,
) -> CompanyBilling {
    let recovered = is_overdue(record) && record.failed_payment_at.is_some();
    CompanyBilling {
        subscription_status: "active".into(),
        payment_status: PaymentStatus::Active,
        failed_payment_at: None,
        grace_period_ends_at: None,
        last_successful_payment_at: paid_at.or(record.last_successful_payment_at),
        recovered_failed_at: if recovered {
            record.failed_payment_at
        } else {
            record.recovered_failed_at
        },
        recovered_at: if recovered {
            Some(now)
        } else {
            record.recovered_at
        },
        ..record.clone()
    }
}

async fn load_record<S: BillingStore>(
    store: &S,
    company_id: Option<i32>,
    customer_id: Option<&str>,
    subscription_id: Option<&str>,
) -> StoreResult<Option<CompanyBilling>> {
    if let Some(company_id) = company_id {
        return Ok(Some(
            store
                .by_company(company_id)
                .await?
                .unwrap_or_else(|| CompanyBilling::empty(company_id)),
        ));
    }
    if let Some(customer_id) = customer_id.filter(|id| !id.is_empty())
        && let Some(record) = store.by_customer(customer_id).await?
    {
        return Ok(Some(record));
    }
    match subscription_id.filter(|id| !id.is_empty()) {
        Some(subscription_id) => store.by_subscription(subscription_id).await,
        None => Ok(None),
    }
}

async fn notify_payment_required<S: BillingStore>(
    store: &S,
    record: &CompanyBilling,
) -> StoreResult<Vec<i32>> {
    let (Some(failed_at), Some(grace_ends)) =
        (record.failed_payment_at, record.grace_period_ends_at)
    else {
        return Ok(Vec::new());
    };
    if !store.billing_required(record.company_id).await? {
        return Ok(Vec::new());
    }
    let mut notified = Vec::new();
    for user_id in store.managers(record.company_id).await? {
        let created = store
            .insert_notification(&BillingNotification {
                company_id: record.company_id,
                user_id,
                dedupe_key: format!("payment_required:{}", iso(failed_at)),
                title: PAYMENT_REQUIRED_TITLE.into(),
                message: payment_required_message(grace_ends),
                action_label: UPDATE_PAYMENT_LABEL.into(),
                href: BILLING_PATH.into(),
            })
            .await?;
        if created {
            notified.push(user_id);
        }
    }
    Ok(notified)
}

async fn save_with_outstanding<S: BillingStore>(
    store: &S,
    record: CompanyBilling,
) -> StoreResult<CompanyBilling> {
    let next = CompanyBilling {
        outstanding_balance_cents: outstanding_balance_cents(
            &store.invoices(record.company_id).await?,
        ),
        ..record
    };
    store.save(&next).await?;
    Ok(next)
}

/// `replaced_by` is set (a plan move replaced this subscription) and the
/// record names another subscription.
fn replaced_by_another(
    record: &CompanyBilling,
    replaced_by: Option<&str>,
    subscription_id: Option<&str>,
) -> bool {
    replaced_by.is_some()
        && record
            .stripe_subscription_id
            .as_deref()
            .is_some_and(|current| Some(current) != subscription_id)
}

/// The subscription a plan invite made (`metadata.plan_invite_id`, not a
/// legacy one it replaced) while the invite is not accepted: only its
/// acceptance applies it.
async fn awaits_acceptance<S: BillingStore>(
    store: &S,
    company_id: i32,
    plan_invite_id: Option<&str>,
    replaced_by: Option<&str>,
) -> StoreResult<bool> {
    let Some(invite_id) = plan_invite_id.filter(|_| replaced_by.is_none()) else {
        return Ok(false);
    };
    Ok(store
        .plan_invite_status(company_id, invite_id)
        .await?
        .is_some_and(|status| status != "accepted"))
}

/// Applies a subscription's state; answers the managers notified. A plan
/// invite's subscription is ignored until the invite is accepted.
pub async fn apply_subscription_change<S: BillingStore>(
    store: &S,
    snapshot: &SubscriptionSnapshot,
    now: NaiveDateTime,
) -> StoreResult<Vec<i32>> {
    apply_subscription(store, snapshot, now, false).await
}

/// [`apply_subscription_change`] for the acceptance of the plan invite that
/// made the subscription, before it marks the invite accepted.
pub async fn apply_invite_subscription<S: BillingStore>(
    store: &S,
    snapshot: &SubscriptionSnapshot,
    now: NaiveDateTime,
) -> StoreResult<Vec<i32>> {
    apply_subscription(store, snapshot, now, true).await
}

async fn apply_subscription<S: BillingStore>(
    store: &S,
    snapshot: &SubscriptionSnapshot,
    now: NaiveDateTime,
    accepting: bool,
) -> StoreResult<Vec<i32>> {
    let Some(record) = load_record(
        store,
        snapshot.company_id,
        Some(&snapshot.customer_id),
        Some(&snapshot.id),
    )
    .await?
    else {
        return Ok(Vec::new());
    };
    if !accepting
        && awaits_acceptance(
            store,
            record.company_id,
            snapshot.plan_invite_id.as_deref(),
            snapshot.replaced_by.as_deref(),
        )
        .await?
    {
        return Ok(Vec::new());
    }
    // A subscription a plan move replaced never takes the record back.
    if replaced_by_another(&record, snapshot.replaced_by.as_deref(), Some(&snapshot.id)) {
        return Ok(Vec::new());
    }
    let status = if snapshot.deleted {
        "canceled"
    } else {
        snapshot.status.as_str()
    };
    // Another live subscription is current: an old one ending changes nothing.
    if has_live_other_subscription(&record, Some(&snapshot.id))
        && !is_paying_subscription_status(status)
    {
        return Ok(Vec::new());
    }
    let merged = merge_subscription(&record, snapshot);
    if is_paying_subscription_status(status) {
        let mut restored = restore_active(&merged, merged.last_successful_payment_at, now);
        restored.subscription_status = status.to_string();
        let saved = save_with_outstanding(store, restored).await?;
        store.clear_payment_notifications(saved.company_id).await?;
        return Ok(Vec::new());
    }
    if matches!(status, "past_due" | "unpaid") {
        let (failed, notify) = mark_past_due(&merged, now);
        let saved = save_with_outstanding(store, failed).await?;
        return match notify {
            Some(_) => notify_payment_required(store, &saved).await,
            None => Ok(Vec::new()),
        };
    }
    if snapshot.deleted || is_ended_subscription_status(status) {
        let in_grace = merged.payment_status == PaymentStatus::PastDue
            && merged.grace_period_ends_at.is_some_and(|ends| now < ends);
        let ended = CompanyBilling {
            subscription_status: status.to_string(),
            payment_status: if in_grace {
                merged.payment_status
            } else {
                PaymentStatus::Suspended
            },
            ..merged
        };
        save_with_outstanding(store, ended).await?;
        return Ok(Vec::new());
    }
    save_with_outstanding(store, merged).await?;
    Ok(Vec::new())
}

/// Applies a paid or failed invoice; answers the managers notified.
pub async fn apply_invoice_change<S: BillingStore>(
    store: &S,
    invoice: &InvoiceSnapshot,
    paid: bool,
    now: NaiveDateTime,
) -> StoreResult<Vec<i32>> {
    let Some(record) = load_record(
        store,
        invoice.company_id,
        Some(&invoice.customer_id),
        invoice.subscription_id.as_deref(),
    )
    .await?
    else {
        return Ok(Vec::new());
    };
    store
        .upsert_invoice(&BillingInvoice {
            company_id: record.company_id,
            stripe_invoice_id: invoice.id.clone(),
            status: if paid { "paid" } else { "open" }.into(),
            amount_due_cents: if paid { 0 } else { invoice.amount_due_cents },
            amount_paid_cents: invoice.amount_paid_cents,
            currency: invoice.currency.clone(),
            hosted_invoice_url: invoice.hosted_invoice_url.clone(),
            invoice_pdf: invoice.invoice_pdf.clone(),
            paid_at: paid.then(|| invoice.paid_at.unwrap_or(now)),
        })
        .await?;
    // An invoice of a subscription a plan move replaced, or of an invite's
    // subscription before its acceptance: recorded, nothing else.
    if awaits_acceptance(
        store,
        record.company_id,
        invoice.plan_invite_id.as_deref(),
        invoice.replaced_by.as_deref(),
    )
    .await?
        || replaced_by_another(
        &record,
        invoice.replaced_by.as_deref(),
        invoice.subscription_id.as_deref(),
    ) {
        save_with_outstanding(store, record).await?;
        return Ok(Vec::new());
    }
    let other_subscription =
        has_live_other_subscription(&record, invoice.subscription_id.as_deref());
    if paid
        && matches!(
            invoice.live_subscription_status.as_deref(),
            Some("past_due" | "unpaid")
        )
    {
        // An older invoice paid (or a late retry) while a newer one is still
        // unpaid: record the payment, keep the grace period running.
        save_with_outstanding(store, record).await?;
        return Ok(Vec::new());
    }
    if paid {
        let mut restored = restore_active(&record, Some(invoice.paid_at.unwrap_or(now)), now);
        if let Some(subscription_id) = &invoice.subscription_id
            && !other_subscription
        {
            restored.stripe_subscription_id = Some(subscription_id.clone());
        }
        if !invoice.customer_id.is_empty() {
            restored.stripe_customer_id = Some(invoice.customer_id.clone());
        }
        let saved = save_with_outstanding(store, restored).await?;
        store.clear_payment_notifications(saved.company_id).await?;
        return Ok(Vec::new());
    }
    if other_subscription {
        save_with_outstanding(store, record).await?;
        return Ok(Vec::new());
    }
    let mut current = record;
    if !invoice.customer_id.is_empty() {
        current.stripe_customer_id = Some(invoice.customer_id.clone());
    }
    if let Some(subscription_id) = &invoice.subscription_id {
        current.stripe_subscription_id = Some(subscription_id.clone());
    }
    let (failed, notify) = mark_past_due(&current, now);
    let saved = save_with_outstanding(store, failed).await?;
    match notify {
        Some(_) => notify_payment_required(store, &saved).await,
        None => Ok(Vec::new()),
    }
}

async fn apply_claimed<S: BillingStore>(
    store: &S,
    event: &BillingEvent,
    now: NaiveDateTime,
) -> StoreResult<(Option<i32>, Vec<i32>)> {
    match event {
        BillingEvent::CheckoutCompleted {
            company_id,
            customer_id,
            subscription_id,
            ..
        } => {
            let record = store
                .by_company(*company_id)
                .await?
                .unwrap_or_else(|| CompanyBilling::empty(*company_id));
            store
                .save(&CompanyBilling {
                    stripe_customer_id: Some(customer_id.clone()),
                    stripe_subscription_id: subscription_id
                        .clone()
                        .or_else(|| record.stripe_subscription_id.clone()),
                    ..record
                })
                .await?;
            Ok((Some(*company_id), Vec::new()))
        }
        BillingEvent::Subscription {
            kind, subscription, ..
        } => {
            let mut snapshot = subscription.clone();
            if *kind == "customer.subscription.deleted" {
                snapshot.deleted = true;
            }
            let notified = apply_subscription_change(store, &snapshot, now).await?;
            let saved = match snapshot.company_id {
                Some(company_id) => store.by_company(company_id).await?,
                None => None,
            };
            let saved = match saved {
                Some(saved) => Some(saved),
                None => store.by_customer(&snapshot.customer_id).await?,
            };
            Ok((saved.map(|record| record.company_id), notified))
        }
        BillingEvent::Invoice { paid, invoice, .. } => {
            let notified = apply_invoice_change(store, invoice, *paid, now).await?;
            let saved = store.by_customer(&invoice.customer_id).await?;
            Ok((saved.map(|record| record.company_id), notified))
        }
        BillingEvent::PaymentMethodUpdated {
            customer_id,
            payment_method_id,
            ..
        } => {
            let Some(record) = store.by_customer(customer_id).await? else {
                return Ok((None, Vec::new()));
            };
            let company_id = record.company_id;
            store
                .save(&CompanyBilling {
                    stripe_payment_method_id: Some(payment_method_id.clone()),
                    ..record
                })
                .await?;
            Ok((Some(company_id), Vec::new()))
        }
    }
}

/// The company an event belongs to, so it is applied under that company's
/// lock; `None` when no company matches (the event then changes nothing).
pub async fn event_company_id<S: BillingStore>(
    store: &S,
    event: &BillingEvent,
) -> StoreResult<Option<i32>> {
    let (company_id, customer_id, subscription_id) = match event {
        BillingEvent::CheckoutCompleted { company_id, .. } => return Ok(Some(*company_id)),
        BillingEvent::Subscription { subscription, .. } => (
            subscription.company_id,
            Some(subscription.customer_id.as_str()),
            Some(subscription.id.as_str()),
        ),
        BillingEvent::Invoice { invoice, .. } => (
            invoice.company_id,
            Some(invoice.customer_id.as_str()),
            invoice.subscription_id.as_deref(),
        ),
        BillingEvent::PaymentMethodUpdated { customer_id, .. } => {
            (None, Some(customer_id.as_str()), None)
        }
    };
    if company_id.is_some() {
        return Ok(company_id);
    }
    Ok(load_record(store, None, customer_id, subscription_id)
        .await?
        .map(|record| record.company_id))
}

/// Applies a Stripe event once: a repeat delivery is a no-op, and an event
/// that fails is released so Stripe's retry runs it again.
pub async fn apply_event<S: BillingStore>(
    store: &S,
    event: &BillingEvent,
    now: NaiveDateTime,
) -> StoreResult<ApplyResult> {
    if !store.claim_event(event.id(), event.event_type()).await? {
        return Ok(ApplyResult {
            duplicate: true,
            company_id: None,
            notified_user_ids: Vec::new(),
        });
    }
    match apply_claimed(store, event, now).await {
        Ok((company_id, notified_user_ids)) => Ok(ApplyResult {
            duplicate: false,
            company_id,
            notified_user_ids,
        }),
        Err(err) => {
            store.release_event(event.id()).await?;
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    //! Port of the CRM's `billingEvents.test.ts` against an in-memory store.
    use super::*;
    use crate::billing::test_store::{Memory, at};

    fn subscription_event(id: &str, company_id: i32, status: &str, deleted: bool) -> BillingEvent {
        BillingEvent::Subscription {
            id: id.into(),
            kind: if deleted {
                "customer.subscription.deleted"
            } else {
                "customer.subscription.updated"
            },
            subscription: SubscriptionSnapshot {
                id: format!("sub_{company_id}"),
                customer_id: format!("cus_{company_id}"),
                price_id: Some("price_monthly".into()),
                item_id: Some(format!("si_{company_id}")),
                status: status.into(),
                quantity: Some(3),
                period_start: Some(at("2026-09-01 00:00:00")),
                period_end: Some(at("2026-10-01 00:00:00")),
                payment_method_id: Some(format!("pm_{company_id}")),
                plan_name: Some("Granite Manager".into()),
                unit_amount_cents: Some(4900),
                company_id: Some(company_id),
                deleted,
                items: Vec::new(),
                monthly_total_cents: None,
                trial_end: None,
                cancel_at: None,
                replaced_by: None,
                plan_invite_id: None,
            },
        }
    }

    fn invoice_event(id: &str, company_id: i32, paid: bool, amount_due: i64) -> BillingEvent {
        BillingEvent::Invoice {
            id: id.into(),
            paid,
            invoice: InvoiceSnapshot {
                id: format!("in_{id}"),
                customer_id: format!("cus_{company_id}"),
                subscription_id: Some(format!("sub_{company_id}")),
                company_id: Some(company_id),
                status: if paid { "paid" } else { "open" }.into(),
                amount_due_cents: amount_due,
                amount_paid_cents: if paid { amount_due } else { 0 },
                currency: "usd".into(),
                hosted_invoice_url: Some("https://pay.stripe.com/invoice/test".into()),
                invoice_pdf: Some("https://pay.stripe.com/invoice/test/pdf".into()),
                paid_at: paid.then(|| at("2026-09-01 00:00:00")),
                live_subscription_status: None,
                replaced_by: None,
                plan_invite_id: None,
            },
        }
    }

    fn active(company_id: i32) -> CompanyBilling {
        CompanyBilling {
            stripe_customer_id: Some(format!("cus_{company_id}")),
            payment_status: PaymentStatus::Active,
            subscription_status: "active".into(),
            ..CompanyBilling::empty(company_id)
        }
    }

    #[tokio::test]
    async fn a_new_subscription_stores_stripe_ids_and_becomes_active_after_payment() {
        let store = Memory::default();
        let now = at("2026-09-01 00:00:00");
        let checkout = BillingEvent::CheckoutCompleted {
            id: "evt_checkout".into(),
            company_id: 1,
            customer_id: "cus_1".into(),
            subscription_id: Some("sub_1".into()),
        };
        apply_event(&store, &checkout, now).await.unwrap();
        apply_event(
            &store,
            &subscription_event("evt_sub", 1, "active", false),
            now,
        )
        .await
        .unwrap();
        apply_event(&store, &invoice_event("evt_paid", 1, true, 14700), now)
            .await
            .unwrap();
        let record = store.record(1);
        assert_eq!(record.stripe_customer_id.as_deref(), Some("cus_1"));
        assert_eq!(record.stripe_subscription_id.as_deref(), Some("sub_1"));
        assert_eq!(record.stripe_price_id.as_deref(), Some("price_monthly"));
        assert_eq!(record.stripe_payment_method_id.as_deref(), Some("pm_1"));
        assert_eq!(record.payment_status, PaymentStatus::Active);
        assert_eq!(record.user_count, Some(3));
        assert_eq!(record.monthly_price_cents, Some(14700));
        assert_eq!(record.grace_period_ends_at, None);
        assert_eq!(store.open_notifications(1), []);
    }

    #[tokio::test]
    async fn a_subscription_event_mirrors_its_trial_end_and_cancellation() {
        let store = Memory::default();
        store.records.borrow_mut().insert(
            1,
            CompanyBilling {
                stripe_subscription_id: Some("sub_1".into()),
                ..active(1)
            },
        );
        let trial_end = at("2027-01-21 00:00:00");
        let with = |id: &str, cancel_at| {
            let mut event = subscription_event(id, 1, "trialing", false);
            if let BillingEvent::Subscription { subscription, .. } = &mut event {
                subscription.trial_end = Some(trial_end);
                subscription.cancel_at = cancel_at;
            }
            event
        };
        let now = at("2026-10-21 00:00:00");

        apply_event(&store, &with("evt_cancel", Some(trial_end)), now)
            .await
            .unwrap();
        let cancelled = store.record(1);
        apply_event(&store, &with("evt_withdraw", None), now)
            .await
            .unwrap();

        assert_eq!(cancelled.trial_end_at, Some(trial_end));
        assert_eq!(cancelled.cancel_at, Some(trial_end));
        assert_eq!(store.record(1).trial_end_at, Some(trial_end));
        assert_eq!(store.record(1).cancel_at, None);
    }

    #[tokio::test]
    async fn a_recurring_payment_keeps_the_company_active_and_stores_the_invoice() {
        let store = Memory::default();
        store.records.borrow_mut().insert(
            1,
            CompanyBilling {
                stripe_subscription_id: Some("sub_1".into()),
                ..active(1)
            },
        );
        apply_event(
            &store,
            &invoice_event("evt_recur", 1, true, 14700),
            at("2026-10-01 00:00:00"),
        )
        .await
        .unwrap();
        let record = store.record(1);
        assert_eq!(record.payment_status, PaymentStatus::Active);
        assert_eq!(
            record.last_successful_payment_at,
            Some(at("2026-09-01 00:00:00"))
        );
        assert_eq!(record.outstanding_balance_cents, 0);
        let invoices = store.invoices.borrow();
        assert_eq!(invoices.len(), 1);
        assert_eq!(invoices[0].status, "paid");
    }

    #[tokio::test]
    async fn a_failed_payment_starts_seven_days_of_grace() {
        let store = Memory::default();
        store.users.borrow_mut().push((10, 1, true));
        store.records.borrow_mut().insert(1, active(1));
        let now = at("2026-09-01 12:00:00");
        apply_event(&store, &invoice_event("evt_fail", 1, false, 14700), now)
            .await
            .unwrap();
        let record = store.record(1);
        assert_eq!(record.payment_status, PaymentStatus::PastDue);
        assert_eq!(record.failed_payment_at, Some(now));
        assert_eq!(record.grace_period_ends_at, Some(add_grace_days(now)));
        assert_eq!(
            with_grace_expiry(&record, now).payment_status,
            PaymentStatus::PastDue
        );
        assert_eq!(record.outstanding_balance_cents, 14700);
    }

    #[tokio::test]
    async fn only_managers_get_the_payment_notification() {
        let store = Memory::default();
        // A manager, then a sales manager, an admin and a sales rep (not managers).
        store.users.borrow_mut().extend([
            (10, 1, true),
            (11, 1, false),
            (12, 1, false),
            (13, 1, false),
        ]);
        let now = at("2026-09-01 12:00:00");
        let result = apply_event(&store, &invoice_event("evt_fail_mgr", 1, false, 1000), now)
            .await
            .unwrap();
        assert_eq!(result.notified_user_ids, [10]);
        let open = store.open_notifications(1);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].user_id, 10);
        assert_eq!(open[0].title, PAYMENT_REQUIRED_TITLE);
        assert_eq!(open[0].action_label, UPDATE_PAYMENT_LABEL);
        assert_eq!(open[0].href, BILLING_PATH);
        assert_eq!(
            open[0].message,
            payment_required_message(add_grace_days(now))
        );
        assert_eq!(
            open[0].dedupe_key,
            "payment_required:2026-09-01T12:00:00.000Z"
        );
    }

    #[tokio::test]
    async fn no_notification_when_billing_is_not_required() {
        let store = Memory::default();
        store.not_required.borrow_mut().insert(1);
        store.users.borrow_mut().push((10, 1, true));
        apply_event(
            &store,
            &invoice_event("evt_fail_off", 1, false, 1000),
            at("2026-09-01 12:00:00"),
        )
        .await
        .unwrap();
        assert_eq!(store.open_notifications(1), []);
    }

    #[tokio::test]
    async fn a_payment_in_grace_restores_access_and_clears_the_warning() {
        let store = Memory::default();
        store.users.borrow_mut().push((10, 1, true));
        apply_event(
            &store,
            &invoice_event("evt_grace_fail", 1, false, 5000),
            at("2026-09-01 00:00:00"),
        )
        .await
        .unwrap();
        assert_eq!(store.open_notifications(1).len(), 1);
        let paid_at = at("2026-09-04 00:00:00");
        apply_event(
            &store,
            &invoice_event("evt_grace_paid", 1, true, 5000),
            paid_at,
        )
        .await
        .unwrap();
        let record = store.record(1);
        assert_eq!(record.payment_status, PaymentStatus::Active);
        assert_eq!(record.grace_period_ends_at, None);
        assert_eq!(record.failed_payment_at, None);
        assert_eq!(store.open_notifications(1), []);
        assert_eq!(
            with_grace_expiry(&record, paid_at).payment_status,
            PaymentStatus::Active
        );
    }

    #[tokio::test]
    async fn a_payment_after_suspension_restores_access() {
        let store = Memory::default();
        store.users.borrow_mut().push((10, 1, true));
        let failed_at = at("2026-09-01 00:00:00");
        apply_event(
            &store,
            &invoice_event("evt_suspend_fail", 1, false, 5000),
            failed_at,
        )
        .await
        .unwrap();
        let suspended = with_grace_expiry(&store.record(1), add_grace_days(failed_at));
        store.save(&suspended).await.unwrap();
        assert_eq!(store.record(1).payment_status, PaymentStatus::Suspended);
        apply_event(
            &store,
            &invoice_event("evt_restore", 1, true, 5000),
            at("2026-09-20 00:00:00"),
        )
        .await
        .unwrap();
        let record = store.record(1);
        assert_eq!(record.payment_status, PaymentStatus::Active);
        assert_eq!(record.subscription_status, "active");
        assert_eq!(record.grace_period_ends_at, None);
        assert_eq!(store.open_notifications(1), []);
    }

    #[tokio::test]
    async fn a_repeated_webhook_changes_nothing() {
        let store = Memory::default();
        store.users.borrow_mut().push((10, 1, true));
        let event = invoice_event("evt_same", 1, false, 2200);
        let now = at("2026-09-01 00:00:00");
        let first = apply_event(&store, &event, now).await.unwrap();
        let second = apply_event(&store, &event, now).await.unwrap();
        assert!(!first.duplicate);
        assert!(second.duplicate);
        assert_eq!(store.notifications.borrow().len(), 1);
        assert_eq!(store.invoices.borrow().len(), 1);
        assert_eq!(store.record(1).outstanding_balance_cents, 2200);
    }

    #[tokio::test]
    async fn one_companys_event_leaves_another_alone() {
        let store = Memory::default();
        store
            .users
            .borrow_mut()
            .extend([(10, 1, true), (20, 2, true)]);
        let other = active(2);
        store.records.borrow_mut().insert(2, other.clone());
        apply_event(
            &store,
            &invoice_event("evt_company_1", 1, false, 9000),
            at("2026-09-01 00:00:00"),
        )
        .await
        .unwrap();
        assert_eq!(store.record(2), other);
        assert_eq!(store.open_notifications(2), []);
        assert_eq!(
            store
                .open_notifications(1)
                .iter()
                .map(|n| n.user_id)
                .collect::<Vec<_>>(),
            [10]
        );
    }

    #[tokio::test]
    async fn an_old_subscription_ending_keeps_the_new_paid_one() {
        let store = Memory::default();
        store.records.borrow_mut().insert(
            1,
            CompanyBilling {
                stripe_subscription_id: Some("sub_new".into()),
                ..active(1)
            },
        );
        let mut event = subscription_event("evt_old_cancel", 1, "canceled", true);
        if let BillingEvent::Subscription { subscription, .. } = &mut event {
            subscription.id = "sub_old".into();
        }
        apply_event(&store, &event, at("2026-09-10 00:00:00"))
            .await
            .unwrap();
        let record = store.record(1);
        assert_eq!(record.payment_status, PaymentStatus::Active);
        assert_eq!(record.stripe_subscription_id.as_deref(), Some("sub_new"));
    }

    #[tokio::test]
    async fn an_old_subscriptions_failed_invoice_starts_no_grace() {
        let store = Memory::default();
        store.users.borrow_mut().push((10, 1, true));
        store.records.borrow_mut().insert(
            1,
            CompanyBilling {
                stripe_subscription_id: Some("sub_new".into()),
                ..active(1)
            },
        );
        let mut event = invoice_event("evt_old_fail", 1, false, 3000);
        if let BillingEvent::Invoice { invoice, .. } = &mut event {
            invoice.subscription_id = Some("sub_old".into());
        }
        apply_event(&store, &event, at("2026-09-10 00:00:00"))
            .await
            .unwrap();
        let record = store.record(1);
        assert_eq!(record.payment_status, PaymentStatus::Active);
        assert_eq!(record.stripe_subscription_id.as_deref(), Some("sub_new"));
        assert_eq!(store.open_notifications(1), []);
    }

    /// Company 1 moved onto a plan: `sub_new` (in `status`) replaced the
    /// legacy `sub_old`.
    fn moved(status: &str) -> Memory {
        let store = Memory::default();
        store.users.borrow_mut().push((10, 1, true));
        store.records.borrow_mut().insert(
            1,
            CompanyBilling {
                stripe_subscription_id: Some("sub_new".into()),
                subscription_status: status.into(),
                trial_end_at: Some(at("2026-11-14 17:00:00")),
                ..active(1)
            },
        );
        store
    }

    fn replaced(event: &mut BillingEvent) {
        match event {
            BillingEvent::Subscription { subscription, .. } => {
                subscription.id = "sub_old".into();
                subscription.replaced_by = Some("sub_new".into());
            }
            BillingEvent::Invoice { invoice, .. } => {
                invoice.subscription_id = Some("sub_old".into());
                invoice.replaced_by = Some("sub_new".into());
            }
            _ => {}
        }
    }

    #[tokio::test]
    async fn a_replaced_legacy_subscription_never_takes_the_record_back() {
        let store = moved("trialing");
        // The legacy subscription, still paid and now cancelling at its period end.
        let mut event = subscription_event("evt_old_update", 1, "active", false);
        replaced(&mut event);
        if let BillingEvent::Subscription { subscription, .. } = &mut event {
            subscription.cancel_at = Some(at("2026-11-14 17:00:00"));
        }
        apply_event(&store, &event, at("2026-10-25 00:00:00"))
            .await
            .unwrap();
        let record = store.record(1);
        assert_eq!(record.stripe_subscription_id.as_deref(), Some("sub_new"));
        assert_eq!(record.subscription_status, "trialing");
        assert_eq!(record.cancel_at, None);
        assert_eq!(record.trial_end_at, Some(at("2026-11-14 17:00:00")));
    }

    #[tokio::test]
    async fn a_replaced_legacy_subscription_ending_changes_nothing_even_after_the_new_one() {
        let store = moved("canceled");
        let mut event = subscription_event("evt_old_end", 1, "canceled", true);
        replaced(&mut event);
        apply_event(&store, &event, at("2026-11-14 17:00:00"))
            .await
            .unwrap();
        let record = store.record(1);
        assert_eq!(record.stripe_subscription_id.as_deref(), Some("sub_new"));
        assert_eq!(record.payment_status, PaymentStatus::Active);
    }

    #[tokio::test]
    async fn invoices_of_a_replaced_subscription_start_no_grace_and_switch_nothing() {
        // Even once the new subscription ended, which disarms the live-subscription check.
        let store = moved("canceled");
        let mut failed = invoice_event("evt_old_failed", 1, false, 30_000);
        replaced(&mut failed);
        apply_event(&store, &failed, at("2026-11-14 18:00:00"))
            .await
            .unwrap();
        let record = store.record(1);
        assert_eq!(record.stripe_subscription_id.as_deref(), Some("sub_new"));
        assert_eq!(record.payment_status, PaymentStatus::Active);
        assert_eq!(record.grace_period_ends_at, None);
        assert_eq!(store.open_notifications(1), []);
        assert_eq!(record.outstanding_balance_cents, 30_000);

        let mut paid = invoice_event("evt_old_paid", 1, true, 30_000);
        replaced(&mut paid);
        apply_event(&store, &paid, at("2026-11-14 19:00:00"))
            .await
            .unwrap();
        let record = store.record(1);
        assert_eq!(record.stripe_subscription_id.as_deref(), Some("sub_new"));
        assert_eq!(record.subscription_status, "canceled");
    }

    #[tokio::test]
    async fn a_new_subscription_takes_over_from_a_past_due_one() {
        let store = Memory::default();
        store.records.borrow_mut().insert(
            1,
            CompanyBilling {
                stripe_subscription_id: Some("sub_old".into()),
                subscription_status: "past_due".into(),
                payment_status: PaymentStatus::Suspended,
                ..active(1)
            },
        );
        let mut event = subscription_event("evt_new_sub", 1, "active", false);
        if let BillingEvent::Subscription { subscription, .. } = &mut event {
            subscription.id = "sub_new".into();
        }
        apply_event(&store, &event, at("2026-09-10 00:00:00"))
            .await
            .unwrap();
        let record = store.record(1);
        assert_eq!(record.payment_status, PaymentStatus::Active);
        assert_eq!(record.stripe_subscription_id.as_deref(), Some("sub_new"));
    }

    /// Company 1 pays legacy `sub_old`; `sub_new` (`status`) is plan invite
    /// `inv-1`'s, which is `invite_status`.
    fn inviting(invite_status: &str, status: &str, deleted: bool) -> (Memory, BillingEvent) {
        let store = Memory::default();
        store.records.borrow_mut().insert(
            1,
            CompanyBilling {
                stripe_subscription_id: Some("sub_old".into()),
                ..active(1)
            },
        );
        store
            .plan_invites
            .borrow_mut()
            .insert("inv-1".into(), (1, invite_status.into()));
        let mut event = subscription_event("evt_invite_sub", 1, status, deleted);
        if let BillingEvent::Subscription { subscription, .. } = &mut event {
            subscription.id = "sub_new".into();
            subscription.plan_invite_id = Some("inv-1".into());
        }
        (store, event)
    }

    #[tokio::test]
    async fn a_plan_invites_subscription_changes_nothing_until_the_invite_is_accepted() {
        let now = at("2026-10-25 00:00:00");
        for invite_status in ["open", "accepting", "revoked"] {
            for (status, deleted) in [("trialing", false), ("canceled", true)] {
                let (store, event) = inviting(invite_status, status, deleted);
                let before = store.record(1);
                apply_event(&store, &event, now).await.unwrap();
                assert_eq!(store.record(1), before, "{invite_status} {status}");
            }
        }
        let (store, event) = inviting("accepted", "trialing", false);
        apply_event(&store, &event, now).await.unwrap();
        let record = store.record(1);
        assert_eq!(record.stripe_subscription_id.as_deref(), Some("sub_new"));
        assert_eq!(record.subscription_status, "trialing");
    }

    #[tokio::test]
    async fn a_plan_invites_trial_invoice_is_only_recorded_until_the_invite_is_accepted() {
        let now = at("2026-10-25 00:00:00");
        for invite_status in ["open", "accepting", "revoked"] {
            // A legacy company without a live subscription of its own.
            let (store, _) = inviting(invite_status, "trialing", false);
            store.records.borrow_mut().insert(
                1,
                CompanyBilling {
                    payment_status: PaymentStatus::Suspended,
                    subscription_status: "canceled".into(),
                    stripe_subscription_id: Some("sub_old".into()),
                    ..active(1)
                },
            );
            let before = store.record(1);
            let mut event = invoice_event("evt_trial_invoice", 1, true, 0);
            if let BillingEvent::Invoice { invoice, .. } = &mut event {
                invoice.subscription_id = Some("sub_new".into());
                invoice.plan_invite_id = Some("inv-1".into());
            }
            apply_event(&store, &event, now).await.unwrap();
            assert_eq!(store.record(1), before, "{invite_status}");
            assert_eq!(store.invoices.borrow().len(), 1, "{invite_status}");
        }
    }

    #[tokio::test]
    async fn the_acceptance_applies_its_own_subscription_and_invites_are_per_company() {
        let now = at("2026-10-25 00:00:00");
        let (store, event) = inviting("accepting", "trialing", false);
        let BillingEvent::Subscription { subscription, .. } = &event else {
            unreachable!()
        };
        apply_invite_subscription(&store, subscription, now)
            .await
            .unwrap();
        assert_eq!(
            store.record(1).stripe_subscription_id.as_deref(),
            Some("sub_new")
        );
        // The invite id is looked up within the company only.
        let (store, event) = inviting("accepting", "trialing", false);
        store
            .plan_invites
            .borrow_mut()
            .insert("inv-1".into(), (2, "accepting".into()));
        apply_event(&store, &event, now).await.unwrap();
        assert_eq!(
            store.record(1).stripe_subscription_id.as_deref(),
            Some("sub_new")
        );
    }

    fn paid_with_live_status(id: &str, company_id: i32, live: &str) -> BillingEvent {
        let mut event = invoice_event(id, company_id, true, 30000);
        if let BillingEvent::Invoice { invoice, .. } = &mut event {
            invoice.live_subscription_status = Some(live.into());
        }
        event
    }

    #[tokio::test]
    async fn the_grace_period_is_seven_days_and_suspends_when_it_ends() {
        let failed_at = at("2026-09-01 00:00:00");
        let record = CompanyBilling {
            payment_status: PaymentStatus::PastDue,
            subscription_status: "past_due".into(),
            failed_payment_at: Some(failed_at),
            grace_period_ends_at: Some(add_grace_days(failed_at)),
            ..CompanyBilling::empty(1)
        };
        assert_eq!(crate::billing::GRACE_PERIOD_DAYS, 7);
        assert_eq!(add_grace_days(failed_at), at("2026-09-08 00:00:00"));
        for day in 0..7 {
            let now = failed_at + chrono::Duration::days(day) + chrono::Duration::minutes(1);
            assert_eq!(
                with_grace_expiry(&record, now).payment_status,
                PaymentStatus::PastDue
            );
        }
        assert_eq!(
            with_grace_expiry(&record, add_grace_days(failed_at)).payment_status,
            PaymentStatus::Suspended
        );
    }

    #[tokio::test]
    async fn sales_managers_admins_and_owners_get_no_notification() {
        let store = Memory::default();
        store
            .users
            .borrow_mut()
            .extend([(11, 1, false), (12, 1, false), (14, 1, false)]);
        let result = apply_event(
            &store,
            &invoice_event("evt_fail_sales", 1, false, 1000),
            at("2026-09-01 12:00:00"),
        )
        .await
        .unwrap();
        assert!(result.notified_user_ids.is_empty());
        assert_eq!(store.open_notifications(1), []);
    }

    #[tokio::test]
    async fn a_checkout_and_a_payment_for_the_same_company_do_not_overwrite_each_other() {
        let store = Memory::default();
        store.records.borrow_mut().insert(
            1,
            CompanyBilling {
                stripe_customer_id: Some("cus_1".into()),
                ..CompanyBilling::empty(1)
            },
        );
        let mut paid = invoice_event("evt_paid", 1, true, 30000);
        if let BillingEvent::Invoice { invoice, .. } = &mut paid {
            invoice.subscription_id = None;
        }
        let checkout = BillingEvent::CheckoutCompleted {
            id: "evt_checkout".into(),
            company_id: 1,
            customer_id: "cus_1".into(),
            subscription_id: Some("sub_new".into()),
        };
        let now = at("2026-09-01 00:00:00");
        // The company lock serialises these in production.
        apply_event(&store, &paid, now).await.unwrap();
        apply_event(&store, &checkout, now).await.unwrap();
        let record = store.record(1);
        assert_eq!(record.payment_status, PaymentStatus::Active);
        assert_eq!(record.stripe_subscription_id.as_deref(), Some("sub_new"));
    }

    #[tokio::test]
    async fn a_late_payment_of_an_old_invoice_keeps_the_grace_of_a_newer_unpaid_one() {
        let store = Memory::default();
        store.users.borrow_mut().push((10, 1, true));
        let failed_at = at("2026-09-01 00:00:00");
        apply_event(
            &store,
            &invoice_event("evt_failed", 1, false, 30000),
            failed_at,
        )
        .await
        .unwrap();
        for live in ["past_due", "unpaid"] {
            let result = apply_event(
                &store,
                &paid_with_live_status(&format!("evt_old_paid_{live}"), 1, live),
                failed_at + chrono::Duration::days(1),
            )
            .await
            .unwrap();
            assert!(result.notified_user_ids.is_empty());
            let record = store.record(1);
            assert_eq!(record.payment_status, PaymentStatus::PastDue);
            assert_eq!(record.grace_period_ends_at, Some(add_grace_days(failed_at)));
            assert_eq!(record.recovered_at, None);
            assert_eq!(store.open_notifications(1).len(), 1);
        }
        let invoices = store.invoices.borrow();
        let paid = invoices
            .iter()
            .find(|i| i.stripe_invoice_id == "in_evt_old_paid_past_due")
            .unwrap();
        assert_eq!(paid.status, "paid");
    }

    #[tokio::test]
    async fn the_payment_that_settles_the_subscription_still_restores_access() {
        let store = Memory::default();
        let failed_at = at("2026-09-01 00:00:00");
        apply_event(
            &store,
            &invoice_event("evt_failed", 1, false, 30000),
            failed_at,
        )
        .await
        .unwrap();
        apply_event(
            &store,
            &paid_with_live_status("evt_paid", 1, "active"),
            failed_at + chrono::Duration::days(1),
        )
        .await
        .unwrap();
        assert_eq!(store.record(1).payment_status, PaymentStatus::Active);
    }

    #[tokio::test]
    async fn the_save_that_restores_access_also_records_which_cycle_recovered() {
        let store = Memory::default();
        let failed_at = at("2026-09-01 00:00:00");
        let paid_at = at("2026-09-03 00:00:00");
        apply_event(
            &store,
            &invoice_event("evt_failed", 1, false, 30000),
            failed_at,
        )
        .await
        .unwrap();
        apply_event(
            &store,
            &paid_with_live_status("evt_paid", 1, "active"),
            paid_at,
        )
        .await
        .unwrap();
        let record = store.record(1);
        assert_eq!(record.payment_status, PaymentStatus::Active);
        assert_eq!(record.recovered_failed_at, Some(failed_at));
        assert_eq!(record.recovered_at, Some(paid_at));
    }

    #[tokio::test]
    async fn an_update_of_an_already_active_subscription_keeps_the_last_recovery() {
        let store = Memory::default();
        let failed_at = at("2026-09-01 00:00:00");
        let recovered_at = at("2026-09-02 00:00:00");
        store.records.borrow_mut().insert(
            1,
            CompanyBilling {
                stripe_subscription_id: Some("sub_1".into()),
                recovered_failed_at: Some(failed_at),
                recovered_at: Some(recovered_at),
                ..active(1)
            },
        );
        apply_event(
            &store,
            &subscription_event("evt_sub_active", 1, "active", false),
            at("2026-09-05 00:00:00"),
        )
        .await
        .unwrap();
        let record = store.record(1);
        assert_eq!(record.recovered_failed_at, Some(failed_at));
        assert_eq!(record.recovered_at, Some(recovered_at));
    }

    #[tokio::test]
    async fn a_cancelled_subscription_that_never_failed_is_not_a_recovery() {
        let store = Memory::default();
        store.records.borrow_mut().insert(
            1,
            CompanyBilling {
                stripe_subscription_id: Some("sub_1".into()),
                subscription_status: "canceled".into(),
                payment_status: PaymentStatus::Suspended,
                ..active(1)
            },
        );
        apply_event(
            &store,
            &subscription_event("evt_sub_back", 1, "active", false),
            at("2026-09-05 00:00:00"),
        )
        .await
        .unwrap();
        let record = store.record(1);
        assert_eq!(record.payment_status, PaymentStatus::Active);
        assert_eq!(record.recovered_failed_at, None);
        assert_eq!(record.recovered_at, None);
    }

    #[tokio::test]
    async fn a_new_failure_starts_a_new_cycle_and_drops_the_old_recovery() {
        let store = Memory::default();
        store.records.borrow_mut().insert(
            1,
            CompanyBilling {
                stripe_subscription_id: Some("sub_1".into()),
                recovered_failed_at: Some(at("2026-09-01 00:00:00")),
                recovered_at: Some(at("2026-09-02 00:00:00")),
                ..active(1)
            },
        );
        let again = at("2026-10-01 00:00:00");
        apply_event(
            &store,
            &invoice_event("evt_failed_again", 1, false, 30000),
            again,
        )
        .await
        .unwrap();
        let record = store.record(1);
        assert_eq!(record.failed_payment_at, Some(again));
        assert_eq!(record.recovered_failed_at, None);
        assert_eq!(record.recovered_at, None);
    }
}
