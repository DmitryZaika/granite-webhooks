//! Ports of `billingEvents.test.ts` and the webhook lock against MySQL.

use super::*;
use api::billing::lock::{CompanyLock, LockError};
use chrono::NaiveDateTime;
use sqlx::mysql::MySqlPoolOptions;

type Cycle = (
    Option<NaiveDateTime>,
    Option<NaiveDateTime>,
    Option<NaiveDateTime>,
);

/// (failed_payment_at, recovered_failed_at, recovered_at) of company 100.
async fn cycle(pool: &MySqlPool) -> Cycle {
    sqlx::query_as(
        "SELECT failed_payment_at, recovered_failed_at, recovered_at
           FROM company_billing WHERE company_id = 100",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn a_paid_invoice_records_which_failure_it_recovered(pool: MySqlPool) {
    manager_and_billing(&pool).await;
    let server = server(&pool, None).await;
    post_event(
        &server,
        &event(
            "evt_fail",
            "invoice.payment_failed",
            invoice("in_1", false, 4900),
        ),
    )
    .await
    .assert_status_ok();
    let (failed_at, recovered_failed_at, recovered_at) = cycle(&pool).await;
    let failed_at = failed_at.unwrap();
    assert_eq!((recovered_failed_at, recovered_at), (None, None));

    post_event(
        &server,
        &event("evt_paid", "invoice.paid", invoice("in_1", true, 4900)),
    )
    .await
    .assert_status_ok();
    let (failed_now, recovered_failed_at, recovered_at) = cycle(&pool).await;
    assert_eq!(failed_now, None);
    assert_eq!(recovered_failed_at, Some(failed_at));
    assert!(recovered_at.is_some());

    // A new failure starts a new cycle and drops the old recovery.
    post_event(
        &server,
        &event(
            "evt_fail_2",
            "invoice.payment_failed",
            invoice("in_2", false, 4900),
        ),
    )
    .await
    .assert_status_ok();
    let (failed_again, recovered_failed_at, recovered_at) = cycle(&pool).await;
    assert!(failed_again.is_some());
    assert_eq!((recovered_failed_at, recovered_at), (None, None));
}

#[test_db::test(seed)]
async fn a_paid_invoice_while_the_subscription_is_still_past_due_keeps_the_grace(pool: MySqlPool) {
    manager_and_billing(&pool).await;
    let stripe = Shared::default();
    {
        let mut stripe = stripe.lock().unwrap();
        stripe
            .invoices
            .insert("in_new".into(), invoice("in_new", false, 4900));
        stripe.subscriptions.insert(
            "sub_100".into(),
            json!({ "id": "sub_100", "object": "subscription", "status": "past_due",
                    "customer": "cus_100", "metadata": { "company_id": "100" },
                    "items": { "data": [] } }),
        );
    }
    let server = server(&pool, Some(&stripe)).await;
    post_event(
        &server,
        &event(
            "evt_fail",
            "invoice.payment_failed",
            invoice("in_new", false, 4900),
        ),
    )
    .await
    .assert_status_ok();

    // An older invoice is paid late; the newer one is still unpaid.
    post_event(
        &server,
        &event(
            "evt_old_paid",
            "invoice.paid",
            invoice("in_old", true, 4900),
        ),
    )
    .await
    .assert_status_ok();
    let row = billing_row(&pool, 100).await.unwrap();
    assert_eq!((row.payment_status.as_str(), row.grace), ("past_due", true));
    assert_eq!(row.outstanding_balance_cents, 4900);
    assert_eq!(open_notifications(&pool).await.len(), 1);
    let (_, recovered_failed_at, recovered_at) = cycle(&pool).await;
    assert_eq!((recovered_failed_at, recovered_at), (None, None));
    let status: String = sqlx::query_scalar(
        "SELECT status FROM company_billing_invoices WHERE stripe_invoice_id = 'in_old'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "paid");

    // Once Stripe says the subscription is active, a payment restores access.
    stripe
        .lock()
        .unwrap()
        .subscriptions
        .get_mut("sub_100")
        .unwrap()["status"] = json!("active");
    post_event(
        &server,
        &event("evt_paid", "invoice.paid", invoice("in_new", true, 4900)),
    )
    .await
    .assert_status_ok();
    let row = billing_row(&pool, 100).await.unwrap();
    assert_eq!((row.payment_status.as_str(), row.grace), ("active", false));
}

fn failed(id: &str, invoice_id: &str, amount: i64) -> Value {
    event(
        id,
        "invoice.payment_failed",
        invoice(invoice_id, false, amount),
    )
}

#[test_db::test(seed)]
async fn a_webhook_waits_for_the_company_lock(pool: MySqlPool) {
    manager_and_billing(&pool).await;
    let server = server(&pool, None).await;
    let lock = CompanyLock::acquire(&pool, 100).await.unwrap();

    let payload = failed("evt_fail", "in_1", 4900);
    let webhook = post_event(&server, &payload);
    let holder = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(billing_row(&pool, 100).await, None);
        lock.release().await;
    };
    let (response, ()) = tokio::join!(webhook, holder);
    response.assert_status_ok();
    assert_eq!(
        billing_row(&pool, 100).await.unwrap().payment_status,
        "past_due"
    );
}

#[test_db::test(seed)]
async fn concurrent_webhooks_for_one_company_both_apply(pool: MySqlPool) {
    manager_and_billing(&pool).await;
    let server = server(&pool, None).await;
    let (a, b) = (failed("evt_a", "in_a", 4900), failed("evt_b", "in_b", 3000));
    let (first, second) = tokio::join!(post_event(&server, &a), post_event(&server, &b));
    first.assert_status_ok();
    second.assert_status_ok();
    let row = billing_row(&pool, 100).await.unwrap();
    assert_eq!(row.payment_status, "past_due");
    assert_eq!(row.outstanding_balance_cents, 7900);
    let invoices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM company_billing_invoices")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(invoices, 2);
    assert_eq!(open_notifications(&pool).await.len(), 1);
}

#[test_db::test(seed)]
async fn the_lock_is_per_company_and_gives_up_when_busy(pool: MySqlPool) {
    let held = CompanyLock::acquire(&pool, 100).await.unwrap();
    let busy = CompanyLock::acquire_within(&pool, 100, 1).await;
    assert!(matches!(busy, Err(LockError::Busy)));
    let other = CompanyLock::acquire_within(&pool, 101, 1).await.unwrap();
    other.release().await;
    held.release().await;
    // Free again once released.
    CompanyLock::acquire_within(&pool, 100, 1)
        .await
        .unwrap()
        .release()
        .await;
}

#[test_db::test(seed)]
async fn a_webhook_works_on_a_single_connection_pool(pool: MySqlPool) {
    manager_and_billing(&pool).await;
    let single = MySqlPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(3))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let server = server(&single, None).await;
    post_event(&server, &failed("evt_fail", "in_1", 4900))
        .await
        .assert_status_ok();
    assert_eq!(
        billing_row(&pool, 100).await.unwrap().payment_status,
        "past_due"
    );
    assert_eq!(open_notifications(&pool).await.len(), 1);
}
