//! The billing reminders store, throttles and `POST /v1/jobs/billing-reminders`.
//! Ports of `billingReminders.server.test.ts` and the job route's test.
//! Seed ids 20200-20249 are this file's.

mod common;

use api::app;
use api::billing::reminder_store::MySqlReminderStore;
use api::billing::reminders::{
    ClaimedReminder, DunningCycle, DunningKind, ReminderKey, ReminderStatus, ReminderStep,
    ReminderStore,
};
use api::billing::stripe::Stripe;
use api::job_runs;
use api::state::AppState;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use chrono::{Duration, NaiveDateTime, SubsecRound, Utc};
use common::{ALLOWED_ORIGIN, SECRET};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

fn dt(text: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
}

fn failed_at() -> NaiveDateTime {
    dt("2026-10-01 15:00:00")
}

fn now() -> NaiveDateTime {
    dt("2026-10-02 15:00:00")
}

/// Whole seconds, like the DATETIME columns.
fn utc_now() -> NaiveDateTime {
    Utc::now().naive_utc().trunc_subsecs(0)
}

fn key(company_id: i32, user_id: i32, kind: DunningKind) -> ReminderKey {
    ReminderKey {
        company_id,
        user_id,
        cycle: DunningCycle {
            kind,
            started_at: failed_at(),
        },
        step: ReminderStep::Started,
    }
}

fn row_of(id: i64, company_id: i32) -> ClaimedReminder {
    ClaimedReminder { id, company_id }
}

async fn companies(pool: &MySqlPool, ids: &[i32]) {
    for id in ids {
        sqlx::query("INSERT INTO company (id, name) VALUES (?, ?)")
            .bind(id)
            .bind(format!("Reminder Stone {id}"))
            .execute(pool)
            .await
            .unwrap();
    }
}

/// A user of `company_id`; `position` 5 is a Manager.
async fn user(pool: &MySqlPool, id: i32, company_id: i32, email: Option<&str>, position: i32) {
    sqlx::query(
        "INSERT INTO users (id, email, password, name, is_employee, is_admin, is_superuser, is_deleted, company_id)
         VALUES (?, ?, 'x', 'Test User', 1, 0, 0, 0, ?)",
    )
    .bind(id)
    .bind(email)
    .bind(company_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO users_positions (user_id, position_id, company_id) VALUES (?, ?, ?)")
        .bind(id)
        .bind(position)
        .bind(company_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn billing_row(
    pool: &MySqlPool,
    company_id: i32,
    payment_status: &str,
    failed: Option<NaiveDateTime>,
    recovered: Option<NaiveDateTime>,
) {
    sqlx::query(
        "INSERT INTO company_billing (company_id, payment_status, subscription_status,
                                      failed_payment_at, recovered_failed_at, recovered_at)
         VALUES (?, ?, 'none', ?, ?, ?)",
    )
    .bind(company_id)
    .bind(payment_status)
    .bind(failed)
    .bind(recovered.map(|_| failed_at()))
    .bind(recovered)
    .execute(pool)
    .await
    .unwrap();
}

async fn status_of(pool: &MySqlPool, id: i64) -> (String, i32) {
    sqlx::query_as("SELECT status, attempts FROM billing_reminder_emails WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn mine(ids: Vec<i32>) -> Vec<i32> {
    ids.into_iter().filter(|id| *id >= 20200).collect()
}

// ---- the store

#[test_db::test(seed)]
async fn the_unique_key_lets_exactly_one_of_several_concurrent_claims_win(pool: MySqlPool) {
    companies(&pool, &[20200]).await;
    let store = MySqlReminderStore(&pool);
    let k = key(20200, 7, DunningKind::PaymentFailed);
    let claims = futures_util::future::join_all((0..5).map(|_| store.claim(&k, now()))).await;
    let won = claims
        .into_iter()
        .filter(|claim| claim.as_ref().unwrap().is_some())
        .count();
    assert_eq!(won, 1);
    assert_eq!(
        store
            .claim(&key(20200, 7, DunningKind::PaymentFailed), now())
            .await
            .unwrap(),
        None
    );
}

#[test_db::test(seed)]
async fn a_first_payment_cycle_never_collides_with_a_failed_payment_cycle(pool: MySqlPool) {
    companies(&pool, &[20200]).await;
    let store = MySqlReminderStore(&pool);
    let failed = store
        .claim(&key(20200, 7, DunningKind::PaymentFailed), now())
        .await
        .unwrap();
    let first = store
        .claim(&key(20200, 7, DunningKind::FirstPayment), now())
        .await
        .unwrap();
    assert!(failed.is_some() && first.is_some());
    assert_ne!(failed, first);
    let rows = store
        .reminders(
            20200,
            DunningCycle {
                kind: DunningKind::FirstPayment,
                started_at: failed_at(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| (Some(row.id), row.key.cycle.kind))
            .collect::<Vec<_>>(),
        vec![(first, DunningKind::FirstPayment)]
    );
}

#[test_db::test(seed)]
async fn a_rejected_send_can_be_claimed_again_until_three_attempts_are_used(pool: MySqlPool) {
    companies(&pool, &[20200]).await;
    let store = MySqlReminderStore(&pool);
    let k = key(20200, 7, DunningKind::PaymentFailed);
    let first = store.claim(&k, now()).await.unwrap().unwrap();
    for attempt in 1..=3 {
        store
            .mark_failed(row_of(first, 20200), "Throttling")
            .await
            .unwrap();
        // The next try waits 5 minutes after the last one.
        let tried = now() + Duration::minutes(5 * i64::from(attempt - 1));
        let too_soon = store.claim(&k, tried + Duration::minutes(4)).await.unwrap();
        assert_eq!(too_soon, None);
        let again = store.claim(&k, tried + Duration::minutes(5)).await.unwrap();
        assert_eq!(again, (attempt < 3).then_some(first));
    }
    assert_eq!(status_of(&pool, first).await, ("failed".to_string(), 3));
}

#[test_db::test(seed)]
async fn sent_and_unknown_rows_are_never_claimed_again(pool: MySqlPool) {
    companies(&pool, &[20200]).await;
    let store = MySqlReminderStore(&pool);
    let sent = store
        .claim(&key(20200, 7, DunningKind::PaymentFailed), now())
        .await
        .unwrap()
        .unwrap();
    let unknown = store
        .claim(&key(20200, 8, DunningKind::PaymentFailed), now())
        .await
        .unwrap()
        .unwrap();
    store
        .mark_sent(row_of(sent, 20200), Some("msg-1"), now())
        .await
        .unwrap();
    store
        .mark_unknown(row_of(unknown, 20200), "SendTimeout")
        .await
        .unwrap();

    for user_id in [7, 8] {
        assert_eq!(
            store
                .claim(&key(20200, user_id, DunningKind::PaymentFailed), now())
                .await
                .unwrap(),
            None
        );
    }
    let rows = store
        .reminders(
            20200,
            DunningCycle {
                kind: DunningKind::PaymentFailed,
                started_at: failed_at(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| (row.key.user_id, row.status))
            .collect::<Vec<_>>(),
        vec![(7, ReminderStatus::Sent), (8, ReminderStatus::Unknown)]
    );
}

#[test_db::test(seed)]
async fn the_sweep_flips_only_old_sending_rows_to_unknown(pool: MySqlPool) {
    companies(&pool, &[20200]).await;
    let store = MySqlReminderStore(&pool);
    let old = store
        .claim(&key(20200, 7, DunningKind::PaymentFailed), now())
        .await
        .unwrap()
        .unwrap();
    let fresh = store
        .claim(
            &key(20200, 8, DunningKind::PaymentFailed),
            now() + Duration::minutes(20),
        )
        .await
        .unwrap()
        .unwrap();

    let swept = store
        .sweep_stuck(now() + Duration::minutes(1))
        .await
        .unwrap();

    assert_eq!(
        swept.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![old]
    );
    assert_eq!(status_of(&pool, old).await.0, "unknown");
    assert_eq!(status_of(&pool, fresh).await.0, "sending");
    // A late answer cannot turn the swept row back into sent.
    store
        .mark_sent(row_of(old, 20200), Some("late"), now())
        .await
        .unwrap();
    assert_eq!(status_of(&pool, old).await.0, "unknown");
}

#[test_db::test(seed)]
async fn dunning_candidates_have_a_fresh_cycle_or_a_recent_recovery(pool: MySqlPool) {
    companies(&pool, &[20200, 20201, 20202, 20203, 20204]).await;
    let [overdue, recovered, not_required, deleted, stale] = [20200, 20201, 20202, 20203, 20204];
    sqlx::query("UPDATE company SET billing_required = 1 WHERE id IN (?, ?, ?, ?)")
        .bind(overdue)
        .bind(recovered)
        .bind(deleted)
        .bind(stale)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE company SET deleted_at = UTC_TIMESTAMP() WHERE id = ?")
        .bind(deleted)
        .execute(&pool)
        .await
        .unwrap();
    for id in [overdue, not_required, deleted] {
        billing_row(&pool, id, "past_due", Some(failed_at()), None).await;
    }
    billing_row(
        &pool,
        stale,
        "past_due",
        Some(now() - Duration::days(15)),
        None,
    )
    .await;
    billing_row(&pool, recovered, "active", None, Some(now())).await;

    let ids = MySqlReminderStore(&pool)
        .candidate_company_ids(now())
        .await
        .unwrap();

    assert_eq!(mine(ids), vec![overdue, recovered]);
}

#[test_db::test(seed)]
async fn candidates_include_companies_asked_to_pay_that_have_not_subscribed(pool: MySqlPool) {
    companies(&pool, &[20200, 20201, 20202, 20203, 20204]).await;
    let [
        never_opened,
        checkout_open,
        already_paying,
        after_email,
        old,
    ] = [20200, 20201, 20202, 20203, 20204];
    for id in [
        never_opened,
        checkout_open,
        already_paying,
        after_email,
        old,
    ] {
        let since = if id == old {
            now() - Duration::days(15)
        } else {
            failed_at()
        };
        sqlx::query(
            "UPDATE company SET billing_required = 1, billing_required_at = ? WHERE id = ?",
        )
        .bind(since)
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }
    billing_row(&pool, checkout_open, "none", None, None).await;
    for id in [already_paying, after_email] {
        billing_row(&pool, id, "active", None, None).await;
    }
    let store = MySqlReminderStore(&pool);
    let claimed = store
        .claim(&key(after_email, 7, DunningKind::FirstPayment), now())
        .await
        .unwrap()
        .unwrap();
    store
        .mark_sent(row_of(claimed, after_email), Some("msg"), now())
        .await
        .unwrap();

    let ids = store.candidate_company_ids(now()).await.unwrap();

    assert_eq!(mine(ids), vec![never_opened, checkout_open, after_email]);
    let snapshot = store.snapshot(never_opened).await.unwrap();
    assert!(snapshot.required);
    assert_eq!(snapshot.required_since, Some(failed_at()));
    assert_eq!(snapshot.record, None);
}

#[test_db::test(seed)]
async fn contacts_are_the_companys_non_deleted_users_with_an_email(pool: MySqlPool) {
    companies(&pool, &[20200, 20201]).await;
    user(&pool, 20200, 20200, Some(" ann@example.com "), 5).await;
    user(&pool, 20201, 20200, Some(""), 5).await;
    user(&pool, 20202, 20200, Some("gone@example.com"), 5).await;
    user(&pool, 20203, 20201, Some("x@example.com"), 5).await;
    sqlx::query("UPDATE users SET is_deleted = 1 WHERE id = 20202")
        .execute(&pool)
        .await
        .unwrap();
    let store = MySqlReminderStore(&pool);

    let contacts = store
        .contacts(20200, &[20200, 20201, 20202, 20203])
        .await
        .unwrap();

    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0].user_id, 20200);
    assert_eq!(contacts[0].email, "ann@example.com");
    assert_eq!(contacts[0].name.as_deref(), Some("Test User"));
    // Managers are the Manager position of that company only.
    assert_eq!(store.managers(20200).await.unwrap(), vec![20200, 20201]);
    assert_eq!(
        store.company_name(20200).await.unwrap(),
        "Reminder Stone 20200"
    );
}

#[test_db::test(seed)]
async fn first_charge_candidates_are_trialing_companies_charged_within_seven_days(pool: MySqlPool) {
    companies(
        &pool,
        &[
            20200, 20201, 20202, 20203, 20204, 20205, 20206, 20207, 20208, 20209,
        ],
    )
    .await;
    let at = utc_now();
    let rows = [
        (20200, "trialing", at + Duration::days(6), None),
        (20201, "trialing", at + Duration::days(2), None),
        (20202, "trialing", at + Duration::days(8), None),
        (20203, "trialing", at - Duration::hours(1), None),
        (20204, "active", at + Duration::days(3), None),
        (20205, "trialing", at + Duration::days(1), None),
        // Cancelled by the charge date: never charged.
        (
            20206,
            "trialing",
            at + Duration::days(3),
            Some(at + Duration::days(3)),
        ),
        // Cancelled after the first charge: still charged.
        (
            20207,
            "trialing",
            at + Duration::days(3),
            Some(at + Duration::days(40)),
        ),
        (20208, "trialing", at + Duration::days(4), None),
        (20209, "trialing", at + Duration::days(5), None),
    ];
    for (id, status, trial_end, cancel_at) in rows {
        sqlx::query(
            "INSERT INTO company_billing (company_id, subscription_status, trial_end_at, cancel_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(status)
        .bind(trial_end)
        .bind(cancel_at)
        .execute(&pool)
        .await
        .unwrap();
    }
    // Stripe said "nothing due" for 20208 an hour ago, for 20209 7 hours ago.
    let store = MySqlReminderStore(&pool);
    for (id, days, hours_ago) in [(20208, 4, 1), (20209, 5, 7)] {
        let cycle = DunningCycle {
            kind: DunningKind::FirstCharge,
            started_at: at + Duration::days(days),
        };
        store
            .skip_cycle(
                id,
                cycle,
                ReminderStep::Upcoming,
                at - Duration::hours(hours_ago),
            )
            .await
            .unwrap();
    }
    sqlx::query("UPDATE company SET deleted_at = UTC_TIMESTAMP() WHERE id = 20205")
        .execute(&pool)
        .await
        .unwrap();

    let ids = MySqlReminderStore(&pool)
        .first_charge_company_ids(at)
        .await
        .unwrap();

    // Soonest first; billing_required plays no part.
    assert_eq!(mine(ids), vec![20201, 20207, 20209, 20200]);
}

// ---- the throttles

#[test_db::test(seed)]
async fn a_job_run_is_claimed_once_per_interval_and_undo_restores_it(pool: MySqlPool) {
    let every = Duration::hours(6);
    let first = job_runs::claim(&pool, "test-throttle", every, now())
        .await
        .unwrap()
        .unwrap();
    assert!(
        job_runs::claim(&pool, "test-throttle", every, now() + Duration::hours(1))
            .await
            .unwrap()
            .is_none()
    );
    first.undo(&pool).await.unwrap();
    let again = job_runs::claim(&pool, "test-throttle", every, now() + Duration::hours(1))
        .await
        .unwrap()
        .unwrap();
    again.finish(&pool, now()).await.unwrap();
    assert!(
        job_runs::claim(&pool, "test-throttle", every, now() + Duration::hours(2))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        job_runs::claim(&pool, "test-throttle", every, now() + Duration::hours(8))
            .await
            .unwrap()
            .is_some()
    );
}

// ---- the route

/// Each `/email/send` body as sent, with the `text` part `OutgoingEmail` drops.
type Mailbox = Arc<Mutex<Vec<Value>>>;

fn trial_subscription(trial_end: NaiveDateTime) -> Value {
    json!({
        "id": "sub_trial", "object": "subscription", "status": "trialing",
        "customer": "cus_trial", "trial_end": trial_end.and_utc().timestamp(),
        "items": { "data": [{ "id": "si_1", "quantity": 1,
            "price": { "id": "price_flat", "nickname": "Granite Manager", "unit_amount": 4900 } }] },
    })
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

/// The job route with a fake mail service and, when given the trial end it
/// reports, a fake Stripe.
async fn server(pool: &MySqlPool, stripe: Option<NaiveDateTime>) -> (TestServer, Mailbox) {
    let mailbox = Mailbox::default();
    let webhooks = serve(
        Router::new()
            .route(
                "/email/send",
                post(
                    |State(mailbox): State<Mailbox>, Json(email): Json<Value>| async move {
                        mailbox.lock().unwrap().push(email);
                        Json(json!({ "message_id": "m-1" }))
                    },
                ),
            )
            .with_state(Arc::clone(&mailbox)),
    )
    .await;
    let mut state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_webhooks(WebhooksClient::new(Some(&webhooks), Some("key")))
        .with_job_key("job-secret")
        .with_app_url("https://app.example.test");
    if let Some(trial_end) = stripe {
        let base = serve(Router::new().route(
            "/v1/subscriptions/{id}",
            get(move |Path(id): Path<String>| async move {
                if id == "sub_trial" {
                    Ok(Json(trial_subscription(trial_end)))
                } else {
                    Err(StatusCode::NOT_FOUND)
                }
            }),
        ))
        .await;
        state = state.with_stripe(Stripe::new(Some("sk_test_fake"), &base));
    }
    (TestServer::new(app(state)), mailbox)
}

fn run(server: &TestServer) -> axum_test::TestRequest {
    server
        .post("/v1/jobs/billing-reminders")
        .add_header("Authorization", "job-secret")
}

fn recipients(mailbox: &Mailbox) -> BTreeSet<String> {
    mailbox
        .lock()
        .unwrap()
        .iter()
        .flat_map(|email| {
            serde_json::from_value::<OutgoingEmail>(email.clone())
                .unwrap()
                .to
        })
        .collect()
}

/// Company `id` was asked to pay an hour ago and never subscribed.
async fn asked_to_pay(pool: &MySqlPool, id: i32) -> NaiveDateTime {
    let since = utc_now() - Duration::hours(1);
    sqlx::query("UPDATE company SET billing_required = 1, billing_required_at = ? WHERE id = ?")
        .bind(since)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    since
}

#[test_db::test(seed)]
async fn the_job_needs_the_job_key(pool: MySqlPool) {
    let (server, _) = server(&pool, None).await;
    server
        .post("/v1/jobs/billing-reminders")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post("/v1/jobs/billing-reminders")
        .add_header("Authorization", "wrong")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn a_due_dunning_reminder_is_sent_once_across_two_calls(pool: MySqlPool) {
    companies(&pool, &[20200, 20201]).await;
    user(&pool, 20200, 20200, Some("boss@reminder.test"), 5).await;
    user(&pool, 20201, 20200, Some("rep@reminder.test"), 1).await;
    user(&pool, 20202, 20201, Some("other@reminder.test"), 5).await;
    asked_to_pay(&pool, 20200).await;
    let (server, mailbox) = server(&pool, None).await;

    let body: Value = run(&server).await.json();

    assert_eq!(body["processed"], 1);
    assert_eq!(body["sent"], 1);
    assert_eq!(body["failed"], 0);
    assert_eq!(body["unknown"], 0);
    assert_eq!(body["company_errors"], 0);
    assert_eq!(body["reconciled"], Value::Null);
    assert_eq!(
        recipients(&mailbox),
        BTreeSet::from(["boss@reminder.test".to_string()])
    );
    let again: Value = run(&server).await.json();
    assert_eq!(again["processed"], 0);
    assert_eq!(mailbox.lock().unwrap().len(), 1);
}

#[test_db::test(seed)]
async fn an_unknown_reminder_is_never_resent(pool: MySqlPool) {
    companies(&pool, &[20200]).await;
    user(&pool, 20200, 20200, Some("boss@reminder.test"), 5).await;
    let since = asked_to_pay(&pool, 20200).await;
    sqlx::query(
        "INSERT INTO billing_reminder_emails
           (company_id, user_id, cycle_kind, cycle_started_at, step, status, attempts, error_code, claimed_at)
         VALUES (20200, 20200, 'first_payment', ?, 'started', 'unknown', 1, 'SendTimeout', ?)",
    )
    .bind(since)
    .bind(since)
    .execute(&pool)
    .await
    .unwrap();
    let (server, mailbox) = server(&pool, None).await;

    let body: Value = run(&server).await.json();

    assert_eq!(body["processed"], 0);
    assert!(mailbox.lock().unwrap().is_empty());
    let status: String =
        sqlx::query_scalar("SELECT status FROM billing_reminder_emails WHERE company_id = 20200")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "unknown");
}

#[test_db::test(seed)]
async fn the_first_charge_email_goes_to_each_manager_once(pool: MySqlPool) {
    companies(&pool, &[20200, 20201]).await;
    let trial_end = utc_now() + Duration::days(5);
    user(&pool, 20200, 20200, Some("one@trial.test"), 5).await;
    user(&pool, 20201, 20200, Some("two@trial.test"), 5).await;
    user(&pool, 20202, 20200, Some("rep@trial.test"), 1).await;
    user(&pool, 20203, 20201, Some("other@trial.test"), 5).await;
    sqlx::query(
        "INSERT INTO company_billing (company_id, stripe_customer_id, stripe_subscription_id,
                                      subscription_status, payment_status, trial_end_at)
         VALUES (20200, 'cus_trial', 'sub_trial', 'trialing', 'none', ?)",
    )
    .bind(trial_end)
    .execute(&pool)
    .await
    .unwrap();
    // Not trialing: another company's manager gets nothing.
    sqlx::query(
        "INSERT INTO company_billing (company_id, subscription_status, payment_status, trial_end_at)
         VALUES (20201, 'active', 'active', ?)",
    )
    .bind(utc_now() + Duration::days(5))
    .execute(&pool)
    .await
    .unwrap();
    let (server, mailbox) = server(&pool, Some(trial_end)).await;

    let body: Value = run(&server).await.json();

    assert_eq!(body["processed"], 2);
    assert_eq!(body["sent"], 2);
    assert_eq!(
        recipients(&mailbox),
        BTreeSet::from(["one@trial.test".to_string(), "two@trial.test".to_string()])
    );
    for sent in mailbox.lock().unwrap().iter() {
        let email: OutgoingEmail = serde_json::from_value(sent.clone()).unwrap();
        assert_eq!(email.to.len(), 1);
        assert!(email.transactional);
        assert!(email.body.contains("<li"));
        let text = sent["text"].as_str().unwrap();
        assert!(text.contains("- Subscription: $49.00"));
        assert!(text.contains("View billing: https://app.example.test/admin/billing"));
    }
    let again: Value = run(&server).await.json();
    assert_eq!(again["processed"], 0);
    assert_eq!(mailbox.lock().unwrap().len(), 2);
}

#[test_db::test(seed)]
async fn old_webhook_events_are_pruned_once_a_day(pool: MySqlPool) {
    for (id, days) in [("evt_20200_old", 100), ("evt_20200_new", 1)] {
        sqlx::query(
            "INSERT INTO stripe_webhook_events (stripe_event_id, event_type, processed_at)
             VALUES (?, 'invoice.paid', UTC_TIMESTAMP() - INTERVAL ? DAY)",
        )
        .bind(id)
        .bind(days)
        .execute(&pool)
        .await
        .unwrap();
    }
    let ids = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT stripe_event_id FROM stripe_webhook_events
              WHERE stripe_event_id LIKE 'evt_20200_%' ORDER BY stripe_event_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    let (server, _) = server(&pool, None).await;

    run(&server).await.assert_status_ok();
    assert_eq!(ids().await, vec!["evt_20200_new".to_string()]);

    // Aged since: the second call is inside the day, so it keeps the row.
    sqlx::query("UPDATE stripe_webhook_events SET processed_at = UTC_TIMESTAMP() - INTERVAL 100 DAY WHERE stripe_event_id = 'evt_20200_new'")
        .execute(&pool)
        .await
        .unwrap();
    run(&server).await.assert_status_ok();
    assert_eq!(ids().await, vec!["evt_20200_new".to_string()]);
}

#[test_db::test(seed)]
async fn the_reconcile_is_stamped_and_not_repeated_within_six_hours(pool: MySqlPool) {
    let (server, _) = server(&pool, Some(utc_now())).await;
    let stamp = || async {
        sqlx::query_scalar::<_, Option<NaiveDateTime>>(
            "SELECT last_finished_at FROM job_runs WHERE job_key = 'billing-reconcile'",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };

    let first: Value = run(&server).await.json();
    assert_eq!(first["reconciled"], json!({ "checked": 0, "failed": 0 }));
    assert!(stamp().await.is_some());

    let second: Value = run(&server).await.json();
    assert_eq!(second["reconciled"], Value::Null);
}
