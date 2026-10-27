//! `POST` / `DELETE /v1/billing/cancellation-request`: a Manager's
//! "Request cancellation" and "Keep my subscription" on the account page,
//! against a fake Stripe and a fake mail and Telegram sink. Ids 20540-20559
//! are this file's.

use crate::common;
use crate::fake_stripe;
use crate::fake_webhooks;

use api::app;
use api::billing::cancellation::resend_confirmations;
use api::billing::stripe::signature_header;
use api::signup::SignupConfig;
use api::state::AppState;
use axum::http::StatusCode;
use axum_test::TestServer;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use common::{ALLOWED_ORIGIN, SECRET, WithSession};
use fake_stripe::{FakeStripe, price};
use fake_webhooks::Sink;
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const WEBHOOK_SECRET: &str = "whsec_cancel_test";
const COMPANY: i32 = 20540;
const OTHER_COMPANY: i32 = 20550;
const SUBSCRIPTION: &str = "sub_cancel";
const SUBSCRIPTION_PATH: &str = "/v1/subscriptions/sub_cancel";
const ROUTE: &str = "/v1/billing/cancellation-request";
const MANAGER_SESSION: &str = "aaaaaaaa-0000-4000-8000-000000020540";
const REP_SESSION: &str = "aaaaaaaa-0000-4000-8000-000000020542";
const OTHER_MANAGER_SESSION: &str = "aaaaaaaa-0000-4000-8000-000000020550";
const MANAGER_ONE: &str = "dana@cancel.test";
const MANAGER_TWO: &str = "max@cancel.test";
const REP: &str = "rep@cancel.test";
const OTHER_MANAGER: &str = "boss@other-cancel.test";
const STAFF_USER: i32 = 101;
const STAFF_EMAIL: &str = "dima@granite.test";
const DAY: i64 = 86_400;
const SUPPORT: &str = "support@granite-manager.com";

/// How the company's subscription stands.
#[derive(Clone, Copy)]
enum Plan {
    Trialing,
    Active,
    Canceled,
    Legacy,
}

struct Fixture {
    pool: MySqlPool,
    state: AppState,
    server: TestServer,
    stripe: fake_stripe::Shared,
    sink: fake_webhooks::Shared,
    trial_end: i64,
    period_end: i64,
}

fn naive(unix: i64) -> NaiveDateTime {
    DateTime::from_timestamp(unix, 0).unwrap().naive_utc()
}

fn unix(value: &Value) -> i64 {
    value
        .as_str()
        .unwrap()
        .parse::<DateTime<Utc>>()
        .unwrap()
        .timestamp()
}

async fn user(pool: &MySqlPool, id: i32, email: &str, name: &str, company: i32, position: i32) {
    sqlx::query(
        "INSERT INTO users (id, email, password, name, is_employee, is_admin, company_id)
         VALUES (?, ?, 'x', ?, 1, 0, ?)",
    )
    .bind(id)
    .bind(email)
    .bind(name)
    .bind(company)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO users_positions (user_id, position_id, company_id) VALUES (?, ?, ?)")
        .bind(id)
        .bind(position)
        .bind(company)
        .execute(pool)
        .await
        .unwrap();
}

async fn session(pool: &MySqlPool, id: &str, user_id: i32) {
    sqlx::query(
        "INSERT INTO sessions (id, user_id, expiration_date, is_deleted, created_date)
         VALUES (?, ?, '2037-12-31 00:00:00', 0, CURRENT_TIMESTAMP)",
    )
    .bind(id)
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn fixture(pool: MySqlPool, plan: Plan) -> Fixture {
    let now = Utc::now().timestamp();
    let trial_end = now + 60 * DAY;
    let period_end = now + 20 * DAY;
    let (status, plan_source) = match plan {
        Plan::Trialing => ("trialing", "subscription"),
        Plan::Active => ("active", "subscription"),
        Plan::Canceled => ("canceled", "subscription"),
        Plan::Legacy => ("active", "legacy"),
    };
    let trialing = matches!(plan, Plan::Trialing);
    let item_end = if trialing { trial_end } else { period_end };
    let stripe = FakeStripe::with_catalog();
    stripe.lock().unwrap().subscriptions.insert(
        SUBSCRIPTION.into(),
        json!({
            "id": SUBSCRIPTION, "object": "subscription", "customer": "cus_cancel",
            "status": status, "trial_end": if trialing { json!(trial_end) } else { Value::Null },
            "cancel_at_period_end": false, "cancel_at": null,
            "metadata": { "company_id": COMPANY.to_string() },
            "items": { "object": "list", "data": [{
                "id": "si_base", "object": "subscription_item", "quantity": 1,
                "price": price("price_base", "granite_base_monthly_v1", 30_000),
                "current_period_start": item_end - 30 * DAY, "current_period_end": item_end,
            }] },
        }),
    );
    sqlx::query(
        "INSERT INTO company (id, name, plan_source)
         VALUES (?, 'Cancel & Stone', ?), (?, 'Other Stone', 'subscription')",
    )
    .bind(COMPANY)
    .bind(plan_source)
    .bind(OTHER_COMPANY)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO company_billing
           (company_id, stripe_customer_id, stripe_subscription_id, subscription_status,
            payment_status, trial_end_at, current_period_end, monthly_price_cents)
         VALUES (?, 'cus_cancel', ?, ?, 'active', ?, ?, 30000)",
    )
    .bind(COMPANY)
    .bind(SUBSCRIPTION)
    .bind(status)
    .bind(trialing.then(|| naive(trial_end)))
    .bind(naive(item_end))
    .execute(&pool)
    .await
    .unwrap();
    user(&pool, 20540, MANAGER_ONE, "Dana Manager", COMPANY, 5).await;
    user(&pool, 20541, MANAGER_TWO, "Max Manager", COMPANY, 5).await;
    user(&pool, 20542, REP, "Rita Rep", COMPANY, 1).await;
    user(&pool, 20550, OTHER_MANAGER, "Bo Boss", OTHER_COMPANY, 5).await;
    session(&pool, MANAGER_SESSION, 20540).await;
    session(&pool, REP_SESSION, 20542).await;
    session(&pool, OTHER_MANAGER_SESSION, 20550).await;
    let sink: fake_webhooks::Shared = Arc::new(Mutex::new(Sink::default()));
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_stripe(fake_stripe::client(&stripe).await)
        .with_webhooks(fake_webhooks::start(&sink).await)
        .with_signup(SignupConfig {
            notify_user_id: Some(STAFF_USER),
            notify_email: Some(STAFF_EMAIL.into()),
            ..Default::default()
        })
        .with_stripe_webhook_secret(WEBHOOK_SECRET);
    Fixture {
        pool,
        state: state.clone(),
        server: TestServer::new(app(state)),
        stripe,
        sink,
        trial_end,
        period_end,
    }
}

impl Fixture {
    async fn request(&self, session: &str, body: Value) -> axum_test::TestResponse {
        self.server
            .post(ROUTE)
            .with_session(session)
            .json(&body)
            .await
    }

    async fn withdraw(&self, session: &str) -> axum_test::TestResponse {
        self.server.delete(ROUTE).with_session(session).await
    }

    fn subscription_calls(&self) -> Vec<fake_stripe::Seen> {
        self.stripe.lock().unwrap().calls("POST", SUBSCRIPTION_PATH)
    }

    fn emails_to(&self, to: &str) -> Vec<Value> {
        self.sink.lock().unwrap().emails_to(to)
    }

    fn email_count(&self) -> usize {
        self.sink.lock().unwrap().emails.len()
    }

    fn telegrams(&self) -> Vec<Value> {
        self.sink.lock().unwrap().telegrams.clone()
    }

    /// `(status, effective_at, requested_by, reason, withdrawn_by, withdrawn)`
    /// of each row, oldest first.
    async fn rows(
        &self,
    ) -> Vec<(
        String,
        NaiveDateTime,
        i32,
        Option<String>,
        Option<i32>,
        bool,
    )> {
        sqlx::query_as(
            "SELECT CAST(status AS CHAR), effective_at, requested_by, reason, withdrawn_by,
                    withdrawn_at IS NOT NULL
               FROM billing_cancellation_requests WHERE company_id = ? ORDER BY id",
        )
        .bind(COMPANY)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// `(requested_at, confirmation_sent_at)` of the latest row.
    async fn confirmation(&self) -> (NaiveDateTime, Option<NaiveDateTime>) {
        sqlx::query_as(
            "SELECT requested_at, confirmation_sent_at FROM billing_cancellation_requests
              WHERE company_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(COMPANY)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// One run of the confirmation resend, as of `now`.
    async fn resend(&self, now: NaiveDateTime) -> usize {
        resend_confirmations(
            &self.state,
            now,
            Instant::now() + std::time::Duration::from_secs(60),
        )
        .await
    }

    fn fail_emails(&self, count: u32) {
        self.sink.lock().unwrap().fail_emails = count;
    }

    async fn cancel_at(&self) -> Option<NaiveDateTime> {
        sqlx::query_scalar("SELECT cancel_at FROM company_billing WHERE company_id = ?")
            .bind(COMPANY)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn post_event(&self, id: &str, kind: &str) {
        let object = self.stripe.lock().unwrap().subscriptions[SUBSCRIPTION].clone();
        let payload = serde_json::to_vec(&json!({
            "id": id, "type": kind, "data": { "object": object },
        }))
        .unwrap();
        let header = signature_header(&payload, WEBHOOK_SECRET, Utc::now().timestamp());
        self.server
            .post("/v1/webhooks/stripe")
            .add_header("stripe-signature", header)
            .add_header("content-type", "application/json")
            .bytes(payload.into())
            .await
            .assert_status_ok();
    }
}

fn text(email: &Value) -> String {
    email["text"].as_str().unwrap().to_string()
}

#[test_db::test(seed)]
async fn a_request_in_the_free_period_ends_at_the_trial_end_and_emails_once(pool: MySqlPool) {
    let f = fixture(pool, Plan::Trialing).await;

    let res = f
        .request(MANAGER_SESSION, json!({ "reason": "  Too expensive  " }))
        .await;

    res.assert_status(StatusCode::CREATED);
    let body: Value = res.json();
    assert_eq!(unix(&body["effective_at"]), f.trial_end);
    assert_eq!(body["requested_by"], "Dana Manager");
    assert_eq!(body["reason"], "Too expensive");
    let calls = f.subscription_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].field("cancel_at_period_end").as_deref(),
        Some("true")
    );
    assert_eq!(
        calls[0].field("cancellation_details[comment]").as_deref(),
        Some("Too expensive")
    );
    assert_eq!(
        calls[0].idempotency_key.as_deref(),
        Some("cancellation:20540:1")
    );
    let rows = f.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "scheduled");
    assert_eq!(rows[0].1, naive(f.trial_end));
    assert_eq!(rows[0].2, 20540);
    assert_eq!(rows[0].3.as_deref(), Some("Too expensive"));
    assert_eq!(f.cancel_at().await, Some(naive(f.trial_end)));

    for manager in [MANAGER_ONE, MANAGER_TWO] {
        let emails = f.emails_to(manager);
        assert_eq!(emails.len(), 1, "{manager}");
        assert_eq!(
            emails[0]["subject"],
            "We received your cancellation request"
        );
        assert_eq!(emails[0]["transactional"], true);
        assert_eq!(emails[0]["from_address"], "noreply@granite-manager.com");
        assert_eq!(emails[0]["reply_to"], json!([SUPPORT]));
        let confirmation = text(&emails[0]);
        assert!(confirmation.contains("reply to this email"));
        assert!(confirmation.contains(
            "Dana Manager requested cancellation of the Granite Manager subscription for Cancel & Stone"
        ));
        assert!(confirmation.contains("not charged at all"));
        assert!(confirmation.contains("Keep my subscription"));
        assert!(
            !confirmation.contains("Too expensive"),
            "the reason is for staff only"
        );
        assert!(
            emails[0]["body"]
                .as_str()
                .unwrap()
                .contains("Cancel &amp; Stone")
        );
    }
    assert!(f.emails_to(REP).is_empty());
    let staff = f.emails_to(STAFF_EMAIL);
    assert_eq!(staff.len(), 1);
    assert!(text(&staff[0]).contains("Company: Cancel & Stone (id 20540)"));
    assert!(text(&staff[0]).contains("Reason: Too expensive"));
    let telegrams = f.telegrams();
    assert_eq!(telegrams.len(), 1);
    assert_eq!(telegrams[0]["user_id"], STAFF_USER);
    let message = telegrams[0]["message"].as_str().unwrap();
    assert!(message.starts_with("Cancellation requested\n"));
    assert!(message.contains("Reason: Too expensive"));

    // A repeat answers the scheduled request: no Stripe call, no email.
    let emails = f.email_count();
    let again = f
        .request(MANAGER_SESSION, json!({ "reason": "other" }))
        .await;
    again.assert_status_ok();
    let again: Value = again.json();
    assert_eq!(again["id"], body["id"]);
    assert_eq!(again["reason"], "Too expensive");
    assert_eq!(f.subscription_calls().len(), 1);
    assert_eq!(f.email_count(), emails);
    assert_eq!(f.telegrams().len(), 1);
    assert_eq!(f.rows().await.len(), 1);
}

#[test_db::test(seed)]
async fn an_active_subscription_ends_at_the_period_end(pool: MySqlPool) {
    let f = fixture(pool, Plan::Active).await;

    let res = f.request(MANAGER_SESSION, json!({ "reason": "   " })).await;

    res.assert_status(StatusCode::CREATED);
    let body: Value = res.json();
    assert_eq!(unix(&body["effective_at"]), f.period_end);
    assert_eq!(body["reason"], Value::Null);
    let calls = f.subscription_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].field("cancellation_details[comment]"), None);
    assert_eq!(f.rows().await[0].3, None);
    assert_eq!(f.cancel_at().await, Some(naive(f.period_end)));
    let confirmation = text(&f.emails_to(MANAGER_ONE)[0]);
    assert!(confirmation.contains("No further subscription charges are made after"));
    assert!(!confirmation.contains("not charged at all"));
    assert!(text(&f.emails_to(STAFF_EMAIL)[0]).contains("Reason: none given"));
}

#[test_db::test(seed)]
async fn withdrawing_keeps_the_subscription_and_a_new_request_gets_a_new_key(pool: MySqlPool) {
    let f = fixture(pool, Plan::Trialing).await;
    let first: Value = f.request(MANAGER_SESSION, json!({})).await.json();
    let id = first["id"].as_i64().unwrap();

    f.withdraw(MANAGER_SESSION)
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let calls = f.subscription_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[1].field("cancel_at_period_end").as_deref(),
        Some("false")
    );
    assert_eq!(
        calls[1].idempotency_key.as_deref(),
        Some(format!("cancellation:20540:{id}:withdraw").as_str())
    );
    let rows = f.rows().await;
    assert_eq!(rows[0].0, "withdrawn");
    assert_eq!(rows[0].4, Some(20540));
    assert!(rows[0].5);
    assert_eq!(f.cancel_at().await, None);
    assert_eq!(
        f.stripe.lock().unwrap().subscriptions[SUBSCRIPTION]["cancel_at"],
        Value::Null
    );
    for manager in [MANAGER_ONE, MANAGER_TWO] {
        let emails = f.emails_to(manager);
        assert_eq!(emails.len(), 2, "{manager}");
        assert_eq!(
            emails[1]["subject"],
            "Your Granite Manager subscription continues"
        );
        assert!(text(&emails[1]).contains("Dana Manager withdrew the cancellation request"));
        assert!(text(&emails[1]).contains("The next charge is on"));
        assert!(text(&emails[1]).contains("Questions? Reply to this email."));
        assert_eq!(emails[1]["reply_to"], json!([SUPPORT]));
    }
    assert_eq!(f.emails_to(STAFF_EMAIL).len(), 2);
    let telegrams = f.telegrams();
    assert_eq!(telegrams.len(), 2);
    assert!(
        telegrams[1]["message"]
            .as_str()
            .unwrap()
            .starts_with("Cancellation withdrawn\n")
    );

    // A repeat is answered as done: no Stripe call, no email.
    let emails = f.email_count();
    f.withdraw(MANAGER_SESSION)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(f.subscription_calls().len(), 2);
    assert_eq!(f.email_count(), emails);

    // A new request is a new one at Stripe too.
    let second = f.request(MANAGER_SESSION, json!({})).await;
    second.assert_status(StatusCode::CREATED);
    let second: Value = second.json();
    assert_ne!(second["id"], first["id"]);
    let calls = f.subscription_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[2].field("cancel_at_period_end").as_deref(),
        Some("true")
    );
    assert_eq!(
        calls[2].idempotency_key.as_deref(),
        Some("cancellation:20540:2")
    );
    let rows = f.rows().await;
    assert_eq!(
        rows.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
        ["withdrawn", "scheduled"]
    );
    assert_eq!(f.cancel_at().await, Some(naive(f.trial_end)));
}

#[test_db::test(seed)]
async fn withdrawing_with_nothing_scheduled_is_a_conflict(pool: MySqlPool) {
    let f = fixture(pool, Plan::Active).await;
    // Never requested.
    f.withdraw(MANAGER_SESSION)
        .await
        .assert_status(StatusCode::CONFLICT);
    assert!(f.stripe.lock().unwrap().requests.is_empty());
    assert_eq!(f.email_count(), 0);

    // The latest request is completed, even after an earlier withdrawal.
    sqlx::query(
        "INSERT INTO billing_cancellation_requests
           (company_id, requested_by, effective_at, status, requested_at)
         VALUES (?, 20540, UTC_TIMESTAMP() - INTERVAL 40 DAY, 'withdrawn',
                 UTC_TIMESTAMP() - INTERVAL 50 DAY),
                (?, 20540, UTC_TIMESTAMP() - INTERVAL 1 DAY, 'completed',
                 UTC_TIMESTAMP() - INTERVAL 20 DAY)",
    )
    .bind(COMPANY)
    .bind(COMPANY)
    .execute(&f.pool)
    .await
    .unwrap();
    f.withdraw(MANAGER_SESSION)
        .await
        .assert_status(StatusCode::CONFLICT);
    assert!(f.stripe.lock().unwrap().requests.is_empty());
    assert_eq!(f.email_count(), 0);
}

#[test_db::test(seed)]
async fn withdrawing_twice_is_fine(pool: MySqlPool) {
    let f = fixture(pool, Plan::Active).await;
    f.request(MANAGER_SESSION, json!({}))
        .await
        .assert_status(StatusCode::CREATED);
    f.withdraw(MANAGER_SESSION)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let emails = f.email_count();
    let telegrams = f.telegrams().len();

    f.withdraw(MANAGER_SESSION)
        .await
        .assert_status(StatusCode::NO_CONTENT);

    assert_eq!(f.subscription_calls().len(), 2);
    assert_eq!(f.email_count(), emails);
    assert_eq!(f.telegrams().len(), telegrams);
    let rows = f.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "withdrawn");
    assert_eq!(f.cancel_at().await, None);
}

#[test_db::test(seed)]
async fn a_delivered_confirmation_is_recorded_at_once_and_never_resent(pool: MySqlPool) {
    let f = fixture(pool, Plan::Active).await;
    f.request(MANAGER_SESSION, json!({}))
        .await
        .assert_status(StatusCode::CREATED);

    let (requested_at, sent_at) = f.confirmation().await;
    assert!(sent_at.is_some_and(|at| at >= requested_at));

    assert_eq!(f.resend(requested_at + Duration::hours(1)).await, 0);
    for manager in [MANAGER_ONE, MANAGER_TWO] {
        assert_eq!(f.emails_to(manager).len(), 1, "{manager}");
    }
}

#[test_db::test(seed)]
async fn a_confirmation_that_reached_no_manager_is_resent_once_after_ten_minutes(pool: MySqlPool) {
    let f = fixture(pool, Plan::Trialing).await;
    f.fail_emails(2);
    f.request(MANAGER_SESSION, json!({ "reason": "Too expensive" }))
        .await
        .assert_status(StatusCode::CREATED);

    let (requested_at, sent_at) = f.confirmation().await;
    assert_eq!(sent_at, None);
    assert!(f.emails_to(MANAGER_ONE).is_empty());
    assert!(f.emails_to(MANAGER_TWO).is_empty());
    assert_eq!(f.emails_to(STAFF_EMAIL).len(), 1);
    assert_eq!(f.telegrams().len(), 1);

    // Too soon: nothing is sent.
    let soon = requested_at + Duration::minutes(9) + Duration::seconds(59);
    assert_eq!(f.resend(soon).await, 0);
    assert!(f.emails_to(MANAGER_ONE).is_empty());
    assert_eq!(f.confirmation().await.1, None);

    let due = requested_at + Duration::minutes(10);
    assert_eq!(f.resend(due).await, 1);
    for manager in [MANAGER_ONE, MANAGER_TWO] {
        let emails = f.emails_to(manager);
        assert_eq!(emails.len(), 1, "{manager}");
        assert_eq!(
            emails[0]["subject"],
            "We received your cancellation request"
        );
        assert_eq!(emails[0]["reply_to"], json!([SUPPORT]));
        let confirmation = text(&emails[0]);
        assert!(confirmation.contains(
            "Dana Manager requested cancellation of the Granite Manager subscription for Cancel & Stone"
        ));
        assert!(confirmation.contains("not charged at all"));
        assert!(!confirmation.contains("Too expensive"));
    }
    assert_eq!(f.confirmation().await.1, Some(due));
    // Staff are not told again.
    assert_eq!(f.emails_to(STAFF_EMAIL).len(), 1);
    assert_eq!(f.telegrams().len(), 1);

    let emails = f.email_count();
    assert_eq!(f.resend(due + Duration::minutes(1)).await, 0);
    assert_eq!(f.email_count(), emails);
}

#[test_db::test(seed)]
async fn a_failed_resend_stays_unrecorded_and_is_retried_on_the_next_run(pool: MySqlPool) {
    let f = fixture(pool, Plan::Active).await;
    f.fail_emails(2);
    f.request(MANAGER_SESSION, json!({}))
        .await
        .assert_status(StatusCode::CREATED);
    let (requested_at, _) = f.confirmation().await;

    f.fail_emails(2);
    let due = requested_at + Duration::minutes(10);
    assert_eq!(f.resend(due).await, 0);
    // Recorded only after a Manager received it: a failed send leaves it unset.
    assert_eq!(f.confirmation().await.1, None);
    assert!(f.emails_to(MANAGER_ONE).is_empty());

    assert_eq!(f.resend(due + Duration::minutes(1)).await, 1);
    assert_eq!(f.confirmation().await.1, Some(due + Duration::minutes(1)));
    assert_eq!(f.emails_to(MANAGER_ONE).len(), 1);
    assert_eq!(f.emails_to(MANAGER_TWO).len(), 1);
    let confirmation = text(&f.emails_to(MANAGER_ONE)[0]);
    assert!(confirmation.contains("No further subscription charges are made after"));
}

#[test_db::test(seed)]
async fn a_confirmation_that_reached_one_manager_counts_as_sent(pool: MySqlPool) {
    let f = fixture(pool, Plan::Active).await;
    f.fail_emails(1);
    f.request(MANAGER_SESSION, json!({}))
        .await
        .assert_status(StatusCode::CREATED);

    let (requested_at, sent_at) = f.confirmation().await;
    assert!(sent_at.is_some());
    assert!(f.emails_to(MANAGER_ONE).is_empty());
    assert_eq!(f.emails_to(MANAGER_TWO).len(), 1);
    assert_eq!(f.resend(requested_at + Duration::hours(1)).await, 0);
    assert_eq!(f.emails_to(MANAGER_TWO).len(), 1);
}

#[test_db::test(seed)]
async fn a_withdrawn_request_is_never_resent(pool: MySqlPool) {
    let f = fixture(pool, Plan::Trialing).await;
    f.fail_emails(2);
    f.request(MANAGER_SESSION, json!({}))
        .await
        .assert_status(StatusCode::CREATED);
    let (requested_at, sent_at) = f.confirmation().await;
    assert_eq!(sent_at, None);
    f.withdraw(MANAGER_SESSION)
        .await
        .assert_status(StatusCode::NO_CONTENT);

    assert_eq!(f.resend(requested_at + Duration::hours(1)).await, 0);
    for manager in [MANAGER_ONE, MANAGER_TWO] {
        let emails = f.emails_to(manager);
        assert_eq!(emails.len(), 1, "{manager}");
        assert_eq!(
            emails[0]["subject"],
            "Your Granite Manager subscription continues"
        );
    }
}

#[test_db::test(seed)]
async fn a_request_that_took_effect_cannot_be_withdrawn(pool: MySqlPool) {
    let f = fixture(pool, Plan::Active).await;
    f.request(MANAGER_SESSION, json!({}))
        .await
        .assert_status(StatusCode::CREATED);
    sqlx::query(
        "UPDATE billing_cancellation_requests SET effective_at = UTC_TIMESTAMP() - INTERVAL 1 HOUR
          WHERE company_id = ?",
    )
    .bind(COMPANY)
    .execute(&f.pool)
    .await
    .unwrap();
    f.withdraw(MANAGER_SESSION)
        .await
        .assert_status(StatusCode::CONFLICT);
    assert_eq!(f.subscription_calls().len(), 1);
    assert_eq!(f.rows().await[0].0, "scheduled");
}

#[test_db::test(seed)]
async fn only_managers_may_request_or_withdraw(pool: MySqlPool) {
    let f = fixture(pool, Plan::Trialing).await;
    f.request(REP_SESSION, json!({}))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    f.withdraw(REP_SESSION)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    f.server
        .post(ROUTE)
        .json(&json!({}))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert!(f.stripe.lock().unwrap().requests.is_empty());
    assert!(f.rows().await.is_empty());
}

#[test_db::test(seed)]
async fn a_legacy_plan_cannot_be_cancelled_here(pool: MySqlPool) {
    let legacy = fixture(pool, Plan::Legacy).await;
    let res = legacy.request(MANAGER_SESSION, json!({})).await;
    res.assert_status(StatusCode::CONFLICT);
    assert!(
        res.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .contains("support@granite-manager.com")
    );
    legacy
        .withdraw(MANAGER_SESSION)
        .await
        .assert_status(StatusCode::CONFLICT);
    assert!(legacy.stripe.lock().unwrap().requests.is_empty());
    assert_eq!(legacy.email_count(), 0);
}

#[test_db::test(seed)]
async fn an_ended_subscription_cannot_be_cancelled(pool: MySqlPool) {
    let f = fixture(pool, Plan::Canceled).await;
    f.request(MANAGER_SESSION, json!({}))
        .await
        .assert_status(StatusCode::CONFLICT);
    assert!(f.stripe.lock().unwrap().requests.is_empty());
    assert!(f.rows().await.is_empty());
}

#[test_db::test(seed)]
async fn a_reason_over_1000_characters_is_refused(pool: MySqlPool) {
    let f = fixture(pool, Plan::Active).await;
    f.request(MANAGER_SESSION, json!({ "reason": "x".repeat(1001) }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    assert!(f.stripe.lock().unwrap().requests.is_empty());
    f.request(MANAGER_SESSION, json!({ "reason": "x".repeat(1000) }))
        .await
        .assert_status(StatusCode::CREATED);
}

#[test_db::test(seed)]
async fn another_companys_manager_cannot_touch_the_request(pool: MySqlPool) {
    let f = fixture(pool, Plan::Trialing).await;
    f.request(MANAGER_SESSION, json!({}))
        .await
        .assert_status(StatusCode::CREATED);

    f.withdraw(OTHER_MANAGER_SESSION)
        .await
        .assert_status(StatusCode::CONFLICT);
    f.request(OTHER_MANAGER_SESSION, json!({}))
        .await
        .assert_status(StatusCode::CONFLICT);

    assert_eq!(f.subscription_calls().len(), 1);
    assert_eq!(f.rows().await[0].0, "scheduled");
    assert_eq!(f.cancel_at().await, Some(naive(f.trial_end)));
    assert!(f.emails_to(OTHER_MANAGER).is_empty());
}

#[test_db::test(seed)]
async fn the_subscription_ending_completes_the_request(pool: MySqlPool) {
    let f = fixture(pool, Plan::Active).await;
    f.request(MANAGER_SESSION, json!({}))
        .await
        .assert_status(StatusCode::CREATED);

    // An update while it is still running leaves it scheduled.
    f.post_event("evt_cancel_1", "customer.subscription.updated")
        .await;
    assert_eq!(f.rows().await[0].0, "scheduled");

    f.stripe
        .lock()
        .unwrap()
        .subscriptions
        .get_mut(SUBSCRIPTION)
        .unwrap()["status"] = json!("canceled");
    f.post_event("evt_cancel_2", "customer.subscription.deleted")
        .await;

    assert_eq!(f.rows().await[0].0, "completed");
    f.withdraw(MANAGER_SESSION)
        .await
        .assert_status(StatusCode::CONFLICT);
}

/// Company 20540 in its onboarding notice week: the deadline passed two
/// days ago, the fee notice went out a day ago and charges in 6 days.
async fn in_the_notice_week(pool: &MySqlPool) {
    let now = naive(Utc::now().timestamp());
    sqlx::query(
        "INSERT INTO company_onboarding_commitments
           (company_id, status, deadline_at, original_deadline_at, fee_cents, notice_sent_at,
            notice_cycle_at, charge_after, created_at, updated_at)
         VALUES (?, 'notice_sent', ?, ?, 60000, ?, ?, ?, ?, ?)",
    )
    .bind(COMPANY)
    .bind(now - Duration::days(2))
    .bind(now - Duration::days(2))
    .bind(now - Duration::days(1))
    .bind(now - Duration::days(1))
    .bind(now + Duration::days(6))
    .bind(now - Duration::days(32))
    .bind(now - Duration::days(1))
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn in_the_notice_week_the_emails_say_what_keeping_the_subscription_does_to_the_fee(
    pool: MySqlPool,
) {
    let f = fixture(pool.clone(), Plan::Active).await;
    in_the_notice_week(&pool).await;

    f.request(MANAGER_SESSION, json!({}))
        .await
        .assert_status(StatusCode::CREATED);
    let confirmation = text(&f.emails_to(MANAGER_ONE)[0]);
    assert!(
        confirmation
            .contains("You requested cancellation before the charge date of the onboarding fee, ")
    );
    assert!(confirmation.contains("If you keep your subscription before "));
    assert!(confirmation.contains(
        "If you keep it later, we first send a new notice at least 7 days before any charge."
    ));

    f.withdraw(MANAGER_SESSION)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let withdrawal = text(&f.emails_to(MANAGER_ONE)[1]);
    assert!(withdrawal.contains(
        "You kept your subscription before the charge date in our onboarding fee notice, so the onboarding fee is charged on "
    ));
}
