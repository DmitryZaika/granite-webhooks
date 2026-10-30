//! Onboarding with Dima: `/v1/onboarding-call`, the superuser routes, the
//! daily job (`onboarding_call::job::run_at` with fixed clocks) and the
//! advance payment (the invoice item on the subscription, the credit once
//! its invoice is paid, the `invoice.paid` webhook), against a fake
//! Calendly, the shared fake Stripe and a fake mail service. Seed ids 20400-20499 are this file's: 20400 pending
//! (Owen Manager 20400, Rita Rep 20401, Ada Admin 20402), 20401 booked (Bea
//! Manager 20403).

use crate::common;
use crate::fake_stripe::{self, FakeStripe};

use api::app;
use api::billing::stripe::signature_header;
use api::onboarding_call::Config;
use api::onboarding_call::calendly::{Calendly, calendly_time};
use api::onboarding_call::commitment::{add_local_days, end_of_local_day, signup_deadline_at};
use api::onboarding_call::job::{self, OnboardingRun};
use api::onboarding_call::store;
use api::signup::SignupConfig;
use api::state::AppState;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use chrono::{Duration, NaiveDateTime};
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

const API: &str = "https://api.calendly.com";
const WEBHOOK_SECRET: &str = "whsec_test_secret";
const OWEN: &str = "aaaaaaaa-0000-4000-8000-000000020400";
const RITA: &str = "aaaaaaaa-0000-4000-8000-000000020401";
const ADA: &str = "aaaaaaaa-0000-4000-8000-000000020402";
const BEA: &str = "aaaaaaaa-0000-4000-8000-000000020403";
const PENDING: i32 = 20400;
const BOOKED: i32 = 20401;
/// `SIGNUP_NOTIFY_EMAIL` and `SIGNUP_NOTIFY_USER_ID` (Dima).
const STAFF_EMAIL: &str = "staff@granite.test";
const STAFF_USER: i32 = 1;
/// The start of the notice's subject.
const PREFIX_NOTICE: &str = "Action needed: your charge on ";
/// `BILLING_ALERT_EMAIL`.
const ALERT_EMAIL: &str = "billing-alerts@granite.test";

// ---- fake Calendly

#[derive(Default)]
struct Cal {
    /// API path -> resource.
    resources: HashMap<String, Value>,
    event_types: Vec<Value>,
    slots: Vec<Value>,
    /// Invitee email -> events (`scheduled_events?invitee_email=`).
    by_email: HashMap<String, Vec<Value>>,
    /// Every path asked for.
    requests: Vec<String>,
}

type SharedCal = Arc<Mutex<Cal>>;

fn params(query: Option<String>) -> HashMap<String, String> {
    serde_html_form::from_str(&query.unwrap_or_default()).unwrap()
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

fn resource(cal: &SharedCal, path: String) -> Result<Json<Value>, StatusCode> {
    let mut cal = cal.lock().unwrap();
    cal.requests.push(path.clone());
    cal.resources
        .get(&path)
        .cloned()
        .map(|resource| Json(json!({ "resource": resource })))
        .ok_or(StatusCode::NOT_FOUND)
}

async fn fake_calendly(cal: SharedCal) -> String {
    let router = Router::new()
        .route(
            "/users/me",
            get(|State(cal): State<SharedCal>| async move {
                cal.lock().unwrap().requests.push("/users/me".into());
                Json(json!({ "resource": {
                    "uri": format!("{API}/users/DIMA"),
                    "scheduling_url": "https://calendly.com/dima",
                }}))
            }),
        )
        .route(
            "/event_types",
            get(|State(cal): State<SharedCal>| async move {
                let mut cal = cal.lock().unwrap();
                cal.requests.push("/event_types".into());
                Json(json!({ "collection": cal.event_types, "pagination": { "next_page": null } }))
            }),
        )
        .route(
            "/event_type_available_times",
            get(|State(cal): State<SharedCal>| async move {
                let mut cal = cal.lock().unwrap();
                cal.requests.push("/event_type_available_times".into());
                Json(json!({ "collection": cal.slots }))
            }),
        )
        .route(
            "/scheduled_events",
            get(|State(cal): State<SharedCal>, RawQuery(query): RawQuery| async move {
                let mut cal = cal.lock().unwrap();
                cal.requests.push("/scheduled_events".into());
                let email = params(query).remove("invitee_email").unwrap_or_default();
                let events = cal.by_email.get(&email).cloned().unwrap_or_default();
                Json(json!({ "collection": events, "pagination": { "next_page": null } }))
            }),
        )
        .route(
            "/scheduled_events/{event}",
            get(|State(cal): State<SharedCal>, Path(event): Path<String>| async move {
                resource(&cal, format!("/scheduled_events/{event}"))
            }),
        )
        .route(
            "/scheduled_events/{event}/invitees",
            get(
                |State(cal): State<SharedCal>, Path(event): Path<String>, RawQuery(query): RawQuery| async move {
                    let mut cal = cal.lock().unwrap();
                    let prefix = format!("/scheduled_events/{event}/invitees/");
                    cal.requests.push(prefix.clone());
                    let email = params(query).remove("email").unwrap_or_default();
                    let invitees: Vec<Value> = cal
                        .resources
                        .iter()
                        .filter(|(path, invitee)| path.starts_with(&prefix) && invitee["email"] == email.as_str())
                        .map(|(_, invitee)| invitee.clone())
                        .collect();
                    Json(json!({ "collection": invitees }))
                },
            ),
        )
        .route(
            "/scheduled_events/{event}/invitees/{invitee}",
            get(
                |State(cal): State<SharedCal>, Path((event, invitee)): Path<(String, String)>| async move {
                    resource(&cal, format!("/scheduled_events/{event}/invitees/{invitee}"))
                },
            ),
        )
        .with_state(cal);
    serve(router).await
}

fn event_resource(id: &str, start: NaiveDateTime, end: NaiveDateTime) -> Value {
    json!({
        "uri": format!("{API}/scheduled_events/{id}"),
        "status": "active",
        "start_time": calendly_time(start),
        "end_time": calendly_time(end),
        "event_type": format!("{API}/event_types/T1"),
        "cancellation": null,
    })
}

fn invitee_resource(event: &str, id: &str, email: &str, tracking: Value) -> Value {
    json!({
        "uri": format!("{API}/scheduled_events/{event}/invitees/{id}"),
        "status": "active",
        "email": email,
        "name": "Owen Manager",
        "tracking": tracking,
        "reschedule_url": format!("https://calendly.com/reschedulings/{id}"),
        "cancel_url": format!("https://calendly.com/cancellations/{id}"),
        "rescheduled": false,
        "new_invitee": null,
        "cancellation": null,
        "no_show": null,
        "event": format!("{API}/scheduled_events/{event}"),
        "created_at": "2026-10-20T10:00:00.000000Z",
    })
}

fn tag(company_id: i32) -> Value {
    json!({ "utm_campaign": "granite-onboarding", "utm_content": format!("company-{company_id}") })
}

/// A call `EVT-<n>` with invitee `INV-<n>` in the fake Calendly; returns the
/// invitee URI.
fn add_call(cal: &SharedCal, n: u32, email: &str, tracking: Value, start: NaiveDateTime) -> String {
    let event = format!("EVT-{n}");
    let invitee = format!("INV-{n}");
    let mut cal = cal.lock().unwrap();
    cal.resources.insert(
        format!("/scheduled_events/{event}"),
        event_resource(&event, start, start + Duration::hours(1)),
    );
    cal.resources.insert(
        format!("/scheduled_events/{event}/invitees/{invitee}"),
        invitee_resource(&event, &invitee, email, tracking),
    );
    format!("{API}/scheduled_events/{event}/invitees/{invitee}")
}

fn edit_invitee(cal: &SharedCal, n: u32, change: impl FnOnce(&mut Value)) {
    let mut cal = cal.lock().unwrap();
    let path = format!("/scheduled_events/EVT-{n}/invitees/INV-{n}");
    change(cal.resources.get_mut(&path).unwrap());
}

// ---- fake Stripe (the shared one) with company 20400's subscription

const CUSTOMER: &str = "cus_seed_onboarding";
const SUBSCRIPTION: &str = "sub_seed_onboarding";

/// Company 20400's trialing subscription in the fake: the base price, the
/// trial end (also the billing cycle anchor) as the seed stored it.
async fn seed_subscription(pool: &MySqlPool, stripe: &fake_stripe::Shared) {
    let trial_end: NaiveDateTime =
        sqlx::query_scalar("SELECT trial_end_at FROM company_billing WHERE company_id = 20400")
            .fetch_one(pool)
            .await
            .unwrap();
    let end = trial_end.and_utc().timestamp();
    let mut fake = stripe.lock().unwrap();
    let mut item = fake.item("price_base", 1, SUBSCRIPTION);
    item["current_period_end"] = json!(end);
    fake.subscriptions.insert(
        SUBSCRIPTION.into(),
        json!({
            "id": SUBSCRIPTION, "object": "subscription", "customer": CUSTOMER,
            "status": "trialing", "trial_end": end, "billing_cycle_anchor": end,
            "cancel_at_period_end": false, "cancel_at": null,
            "metadata": { "company_id": "20400" },
            "items": { "object": "list", "data": [item] },
        }),
    );
}

// ---- mail and the harness

/// Each `/email/send` body as sent.
type Mailbox = Arc<Mutex<Vec<Value>>>;

struct Harness {
    server: TestServer,
    state: AppState,
    cal: SharedCal,
    stripe: fake_stripe::Shared,
    mailbox: Mailbox,
}

#[derive(Clone, Copy)]
struct Options {
    calendly: bool,
    charging: bool,
}

const ON: Options = Options {
    calendly: true,
    charging: true,
};

async fn harness(pool: &MySqlPool, options: Options) -> Harness {
    let cal = SharedCal::default();
    {
        let mut c = cal.lock().unwrap();
        c.event_types = vec![json!({
            "uri": format!("{API}/event_types/T1"),
            "scheduling_url": "https://calendly.com/dima/onboarding",
            "active": true,
        })];
        c.slots = vec![json!({ "status": "available", "start_time": "2030-01-02T15:00:00Z" })];
    }
    let calendly_base = fake_calendly(Arc::clone(&cal)).await;
    let stripe = FakeStripe::with_catalog();
    seed_subscription(pool, &stripe).await;
    let stripe_client = fake_stripe::client(&stripe).await;
    let mailbox = Mailbox::default();
    let telegrams = Mailbox::default();
    let webhooks =
        serve(
            Router::new()
                .route(
                    "/email/send",
                    post(
                        |State((mailbox, _)): State<(Mailbox, Mailbox)>,
                         Json(email): Json<Value>| async move {
                            mailbox.lock().unwrap().push(email);
                            Json(json!({ "message_id": "m-1" }))
                        },
                    ),
                )
                .route(
                    "/telegram/notifications-notify",
                    post(
                        |State((_, telegrams)): State<(Mailbox, Mailbox)>,
                         Json(message): Json<Value>| async move {
                            telegrams.lock().unwrap().push(message);
                            Json(json!({ "ok": true }))
                        },
                    ),
                )
                .with_state((Arc::clone(&mailbox), Arc::clone(&telegrams))),
        )
        .await;
    let calendly = if options.calendly {
        Calendly::new(Some("fake-token"), &calendly_base, None)
    } else {
        Calendly::disabled()
    };
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_webhooks(WebhooksClient::new(Some(&webhooks), Some("key")))
        .with_job_key("job-secret")
        .with_app_url("https://app.example.test")
        .with_stripe(stripe_client)
        .with_billing_alert_email(ALERT_EMAIL)
        .with_stripe_webhook_secret(WEBHOOK_SECRET)
        .with_signup(SignupConfig {
            notify_user_id: Some(STAFF_USER),
            notify_email: Some(Arc::from(STAFF_EMAIL)),
            ..SignupConfig::default()
        })
        .with_onboarding_call(Config {
            calendly,
            advance_charging: options.charging,
        });
    Harness {
        server: TestServer::new(app(state.clone())),
        state,
        cal,
        stripe,
        mailbox,
    }
}

impl Harness {
    /// One job run at `now`. The days the clock skipped get an `available`
    /// check, as a job running daily would have recorded (missed days toll).
    async fn run(&self, now: NaiveDateTime) -> OnboardingRun {
        let pool = &self.state.pool;
        let first: Option<chrono::NaiveDate> =
            sqlx::query_scalar("SELECT MIN(DATE(created_at)) FROM company_onboarding_commitments")
                .fetch_one(pool)
                .await
                .unwrap();
        // From the day before: the local signup day can be the UTC day before.
        let mut day = first.unwrap_or_else(|| now.date()) - Duration::days(1);
        while day < now.date() {
            sqlx::query(
                "INSERT IGNORE INTO onboarding_availability_days
                   (checked_on, checked_at, result, slots, window_start_at, window_end_at)
                 VALUES (?, ?, 'available', 1, ?, ?)",
            )
            .bind(day)
            .bind(day.and_hms_opt(0, 0, 0).unwrap())
            .bind(day.and_hms_opt(0, 0, 0).unwrap())
            .bind(day.and_hms_opt(0, 0, 0).unwrap())
            .execute(pool)
            .await
            .unwrap();
            day = day.succ_opt().unwrap();
        }
        job::run_at(&self.state, now).await
    }

    fn subjects_to(&self, email: &str) -> Vec<String> {
        self.mailbox
            .lock()
            .unwrap()
            .iter()
            .filter_map(|sent| serde_json::from_value::<OutgoingEmail>(sent.clone()).ok())
            .filter(|sent| sent.to.iter().any(|to| to == email))
            .map(|sent| sent.subject)
            .collect()
    }

    fn bodies_to(&self, email: &str) -> Vec<String> {
        self.mailbox
            .lock()
            .unwrap()
            .iter()
            .filter_map(|sent| serde_json::from_value::<OutgoingEmail>(sent.clone()).ok())
            .filter(|sent| sent.to.iter().any(|to| to == email))
            .map(|sent| sent.body)
            .collect()
    }

    /// The reply-to addresses of each email to `email`.
    fn reply_tos(&self, email: &str) -> Vec<Vec<String>> {
        self.mailbox
            .lock()
            .unwrap()
            .iter()
            .filter_map(|sent| serde_json::from_value::<OutgoingEmail>(sent.clone()).ok())
            .filter(|sent| sent.to.iter().any(|to| to == email))
            .map(|sent| sent.reply_to)
            .collect()
    }

    /// Subjects of the advance notices to `email`.
    fn notices_to(&self, email: &str) -> Vec<String> {
        self.subjects_to(email)
            .into_iter()
            .filter(|subject| subject.starts_with(PREFIX_NOTICE))
            .collect()
    }

    fn recipients(&self) -> BTreeSet<String> {
        self.mailbox
            .lock()
            .unwrap()
            .iter()
            .flat_map(|sent| {
                serde_json::from_value::<OutgoingEmail>(sent.clone())
                    .unwrap()
                    .to
            })
            .collect()
    }

    async fn book(&self, session: &str, invitee_uri: &str) -> axum_test::TestResponse {
        self.server
            .post("/v1/onboarding-call/bookings")
            .with_session(session)
            .json(&json!({ "invitee_uri": invitee_uri }))
            .await
    }
}

/// The company's commitment as stored.
async fn commitment(
    pool: &MySqlPool,
    company_id: i32,
) -> api::onboarding_call::commitment::Commitment {
    store::load(pool, company_id)
        .await
        .unwrap()
        .unwrap()
        .commitment
}

async fn status(pool: &MySqlPool, company_id: i32) -> String {
    sqlx::query_scalar(
        "SELECT CAST(status AS CHAR) FROM company_onboarding_commitments WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn event_kinds(pool: &MySqlPool, company_id: i32) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT kind FROM company_onboarding_commitment_events WHERE company_id = ? ORDER BY id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Only company 20400's commitment is open (20401's is settled).
async fn only_pending(pool: &MySqlPool) {
    sqlx::query(
        "UPDATE company_onboarding_commitments SET status = 'completed' WHERE company_id = ?",
    )
    .bind(BOOKED)
    .execute(pool)
    .await
    .unwrap();
}

/// Signup and deadline of company 20400.
async fn clock(pool: &MySqlPool) -> (NaiveDateTime, NaiveDateTime) {
    let c = commitment(pool, PENDING).await;
    (c.created_at, c.deadline_at)
}

async fn post_event(server: &TestServer, event: &Value) -> axum_test::TestResponse {
    let payload = serde_json::to_vec(event).unwrap();
    let header = signature_header(&payload, WEBHOOK_SECRET, chrono::Utc::now().timestamp());
    server
        .post("/v1/webhooks/stripe")
        .add_header("stripe-signature", header)
        .add_header("content-type", "application/json")
        .bytes(payload.into())
        .await
}

// ---- GET /v1/onboarding-call and POST /v1/onboarding-call/bookings

#[test_db::test(seed)]
async fn without_a_session_the_routes_answer_401(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    h.server
        .get("/v1/onboarding-call")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    h.server
        .post("/v1/onboarding-call/bookings")
        .json(&json!({ "invitee_uri": format!("{API}/scheduled_events/A/invitees/B") }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn a_rep_sees_the_commitment_but_cannot_book(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let body: Value = h
        .server
        .get("/v1/onboarding-call")
        .with_session(RITA)
        .await
        .json();
    assert_eq!(body["commitment"]["status"], "pending");
    assert_eq!(body["commitment"]["signup_advance_cents"], 30_000);
    assert_eq!(body["commitment"]["advance_cents"], Value::Null);
    assert_eq!(body["can_book"], false);
    assert_eq!(body["booking_url"], Value::Null);
    assert_eq!(body["booking_unavailable"], false);
    let uri = add_call(
        &h.cal,
        1,
        "rep@onboarding.test",
        tag(PENDING),
        clock(&pool).await.0 + Duration::days(5),
    );
    h.book(RITA, &uri)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    assert_eq!(status(&pool, PENDING).await, "pending");
}

#[test_db::test(seed)]
async fn a_manager_gets_a_tagged_booking_link(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let body: Value = h
        .server
        .get("/v1/onboarding-call")
        .with_session(OWEN)
        .await
        .json();
    assert_eq!(body["can_book"], true);
    assert_eq!(body["booking_unavailable"], false);
    assert_eq!(
        body["booking_url"],
        "https://calendly.com/dima?name=Owen+Manager&email=manager%40onboarding.test&utm_campaign=granite-onboarding&utm_content=company-20400"
    );
    let commitment = &body["commitment"];
    assert!(
        commitment["deadline_at"]
            .as_str()
            .unwrap()
            .ends_with(".000Z")
    );
    assert_eq!(commitment["booked_start_at"], Value::Null);

    // Without Calendly the page cannot be resolved.
    let off = harness(
        &pool,
        Options {
            calendly: false,
            charging: true,
        },
    )
    .await;
    let body: Value = off
        .server
        .get("/v1/onboarding-call")
        .with_session(OWEN)
        .await
        .json();
    assert_eq!(body["booking_url"], Value::Null);
    assert_eq!(body["booking_unavailable"], true);
}

#[test_db::test(seed)]
async fn an_admin_who_is_not_a_manager_books_through_calendly(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let start = clock(&pool).await.0 + Duration::days(5);
    let uri = add_call(&h.cal, 1, "admin@onboarding.test", tag(PENDING), start);
    let response = h.book(ADA, &uri).await;
    response.assert_status_ok();
    let body: Value = response.json();
    assert_eq!(body["commitment"]["status"], "booked");
    assert_eq!(
        body["commitment"]["booked_start_at"],
        api::billing::iso(start)
    );
    assert_eq!(
        body["commitment"]["reschedule_url"],
        "https://calendly.com/reschedulings/INV-1"
    );
    let stored = commitment(&pool, PENDING).await;
    let booking = stored.booking.unwrap();
    assert_eq!(booking.invitee_uri, uri);
    assert_eq!(booking.end_at, start + Duration::hours(1));
    assert_eq!(event_kinds(&pool, PENDING).await, ["created", "booked"]);
    let requests = h.cal.lock().unwrap().requests.clone();
    assert!(requests.contains(&"/scheduled_events/EVT-1/invitees/INV-1".to_string()));
    assert!(requests.contains(&"/scheduled_events/EVT-1".to_string()));
}

#[test_db::test(seed)]
async fn a_manager_of_another_company_never_sees_or_books_this_one(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let body: Value = h
        .server
        .get("/v1/onboarding-call")
        .with_session(BEA)
        .await
        .json();
    assert_eq!(body["commitment"]["status"], "booked");
    assert!(
        body["booking_url"]
            .as_str()
            .unwrap()
            .contains("utm_content=company-20401")
    );
    let start = clock(&pool).await.0 + Duration::days(5);
    let uri = add_call(&h.cal, 1, "manager@booked.test", tag(PENDING), start);
    h.book(BEA, &uri)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(status(&pool, PENDING).await, "pending");
    assert_eq!(
        commitment(&pool, BOOKED).await.booking.unwrap().invitee_uri,
        "https://api.calendly.com/scheduled_events/SEED-EVT-20401/invitees/SEED-INV-20401"
    );
}

#[test_db::test(seed)]
async fn another_company_tag_or_a_missing_campaign_is_rejected(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let start = clock(&pool).await.0 + Duration::days(5);
    let other = add_call(&h.cal, 1, "manager@onboarding.test", tag(BOOKED), start);
    h.book(OWEN, &other)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let untagged = add_call(
        &h.cal,
        2,
        "manager@onboarding.test",
        json!({ "utm_campaign": null, "utm_content": "company-20400" }),
        start,
    );
    h.book(OWEN, &untagged)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let cancelled = add_call(&h.cal, 3, "manager@onboarding.test", tag(PENDING), start);
    edit_invitee(&h.cal, 3, |invitee| invitee["status"] = json!("canceled"));
    h.book(OWEN, &cancelled)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(status(&pool, PENDING).await, "pending");
}

#[test_db::test(seed)]
async fn a_uri_on_another_host_is_refused_without_asking_anyone(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    for uri in [
        "https://evil.test/scheduled_events/EVT-1/invitees/INV-1",
        "https://api.calendly.com.evil.test/scheduled_events/EVT-1/invitees/INV-1",
        "https://api.calendly.com/users/me",
        "https://api.calendly.com/scheduled_events/EVT-1/invitees/INV-1?x=1",
    ] {
        h.book(OWEN, uri)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    assert!(h.cal.lock().unwrap().requests.is_empty());
}

// ---- the superuser routes

#[test_db::test(seed)]
async fn the_superuser_routes_are_for_superusers_only(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let path = "/v1/superuser/companies/20400/onboarding-commitment";
    h.server
        .get(path)
        .with_session(OWEN)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    h.server
        .put(path)
        .with_session(OWEN)
        .json(&json!({ "action": "waive" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    assert_eq!(status(&pool, PENDING).await, "pending");
}

#[test_db::test(seed)]
async fn superuser_actions_are_stamped_with_the_actor_and_time(pool: MySqlPool) {
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(&pool)
        .await
        .unwrap();
    let name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = 102")
        .fetch_one(&pool)
        .await
        .unwrap();
    let h = harness(&pool, ON).await;
    let path = "/v1/superuser/companies/20400/onboarding-commitment";
    let (_, deadline) = clock(&pool).await;

    let body: Value = h
        .server
        .get(path)
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(body["commitment"]["company_id"], PENDING);
    assert_eq!(body["commitment"]["status"], "pending");
    assert_eq!(body["charging_enabled"], true);
    assert_eq!(body["events"][0]["kind"], "created");

    let extended = h
        .server
        .put(path)
        .with_session(sessions::ADMIN)
        .json(&json!({ "action": "extend" }))
        .await;
    extended.assert_status_ok();
    let body: Value = extended.json();
    assert_eq!(
        body["commitment"]["deadline_at"],
        api::billing::iso(deadline + Duration::days(14))
    );
    assert_eq!(body["commitment"]["extension_used"], true);
    let event = &body["events"][0];
    assert_eq!(event["kind"], "extended");
    assert_eq!(event["actor_user_id"], 102);
    assert_eq!(event["actor_name"], name.as_str());
    assert!(event["created_at"].as_str().is_some());
    assert_eq!(event["detail"]["days_added"], 14);

    let again = h
        .server
        .put(path)
        .with_session(sessions::ADMIN)
        .json(&json!({ "action": "extend" }))
        .await;
    again.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        again.json::<Value>()["error"],
        "The 14-day extension was already used."
    );
    h.server
        .put(path)
        .with_session(sessions::ADMIN)
        .json(&json!({ "action": "no_show" }))
        .await
        .assert_status(StatusCode::CONFLICT);

    let waived: Value = h
        .server
        .put(path)
        .with_session(sessions::ADMIN)
        .json(&json!({ "action": "waive" }))
        .await
        .json();
    assert_eq!(waived["commitment"]["status"], "waived");
    assert_eq!(waived["events"][0]["actor_user_id"], 102);
    h.server
        .put(path)
        .with_session(sessions::ADMIN)
        .json(&json!({ "action": "complete" }))
        .await
        .assert_status(StatusCode::CONFLICT);
    h.server
        .put("/v1/superuser/companies/100/onboarding-commitment")
        .with_session(sessions::ADMIN)
        .json(&json!({ "action": "waive" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let none: Value = h
        .server
        .get("/v1/superuser/companies/100/onboarding-commitment")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(none["commitment"], Value::Null);
}

// ---- the daily job

#[test_db::test(seed)]
async fn the_job_needs_the_job_key(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    h.server
        .post("/v1/jobs/onboarding-commitments")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    let res = h
        .server
        .post("/v1/jobs/onboarding-commitments")
        .add_header("Authorization", "job-secret")
        .await;
    res.assert_status_ok();
    // time-triggered logs `processed` from every job.
    let body: Value = res.json();
    let work = [
        "reminders",
        "notices",
        "advance_items",
        "credits",
        "tolled",
        "completed",
    ]
    .iter()
    .map(|key| body[key].as_u64().unwrap())
    .sum::<u64>();
    assert_eq!(body["processed"].as_u64(), Some(work));
}

#[test_db::test(seed)]
async fn reminders_go_once_to_each_manager_across_two_runs(pool: MySqlPool) {
    only_pending(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO users (id, email, password, name, is_employee, is_admin, is_superuser, is_deleted, company_id)
         VALUES (20404, 'second@onboarding.test', 'x', 'Second Manager', 1, 0, 0, 0, 20400);
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (20404, 5, 20400);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let h = harness(&pool, ON).await;
    let (created, _) = clock(&pool).await;

    let first = h
        .run(created + Duration::days(7) + Duration::hours(1))
        .await;
    let second = h
        .run(created + Duration::days(8) + Duration::hours(2))
        .await;

    assert_eq!(first.reminders, 2);
    assert_eq!(second.reminders, 0);
    assert_eq!(second.companies, 1);
    assert_eq!(
        h.recipients(),
        BTreeSet::from([
            "manager@onboarding.test".to_string(),
            "second@onboarding.test".to_string()
        ])
    );
    assert_eq!(
        h.subjects_to("manager@onboarding.test"),
        ["Book your free Granite Manager onboarding call"]
    );
    // It asks people to reply: replies go to support, not to noreply.
    assert_eq!(
        h.reply_tos("manager@onboarding.test"),
        [vec!["support@granite-manager.com".to_string()]]
    );
    let rows: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT user_id, step, status FROM billing_reminder_emails
          WHERE company_id = 20400 AND cycle_kind = 'onboarding' ORDER BY user_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            (20400, "day_7".to_string(), "sent".to_string()),
            (20404, "day_7".to_string(), "sent".to_string())
        ]
    );
}

#[test_db::test(seed)]
async fn a_day_without_a_slot_moves_the_deadline_once(pool: MySqlPool) {
    only_pending(&pool).await;
    let h = harness(&pool, ON).await;
    h.cal.lock().unwrap().slots.clear();
    let (created, deadline) = clock(&pool).await;

    let run = h.run(created + Duration::days(1)).await;

    assert_eq!(run.tolled, 1);
    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.deadline_at, add_local_days(deadline, 1));
    assert_eq!(c.tolled_days, 1);
    let day: (String, i32) = sqlx::query_as(
        "SELECT result, slots FROM onboarding_availability_days WHERE checked_on = ?",
    )
    .bind((created + Duration::days(1)).date())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(day, ("no_slots".to_string(), 0));

    // A slot the next day: no more tolling.
    h.cal
        .lock()
        .unwrap()
        .slots
        .push(json!({ "status": "available", "start_time": "2030-01-02T15:00:00Z" }));
    let next = h.run(created + Duration::days(2)).await;
    assert_eq!(next.tolled, 0);
    assert_eq!(commitment(&pool, PENDING).await.tolled_days, 1);
}

#[test_db::test(seed)]
async fn an_unconfigured_calendly_tolls_too(pool: MySqlPool) {
    only_pending(&pool).await;
    let h = harness(
        &pool,
        Options {
            calendly: false,
            charging: true,
        },
    )
    .await;
    let (created, deadline) = clock(&pool).await;

    let run = h.run(created + Duration::days(1)).await;

    assert_eq!(run.tolled, 1);
    assert_eq!(
        commitment(&pool, PENDING).await.deadline_at,
        add_local_days(deadline, 1)
    );
    let result: String =
        sqlx::query_scalar("SELECT result FROM onboarding_availability_days WHERE checked_on = ?")
            .bind((created + Duration::days(1)).date())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(result, "not_configured");
}

/// Owen books call 1 (5 days after signup) through the widget; its end.
async fn booked_call(h: &Harness, pool: &MySqlPool) -> NaiveDateTime {
    only_pending(pool).await;
    let start = clock(pool).await.0 + Duration::days(5);
    let uri = add_call(&h.cal, 1, "manager@onboarding.test", tag(PENDING), start);
    h.book(OWEN, &uri).await.assert_status_ok();
    start + Duration::hours(1)
}

#[test_db::test(seed)]
async fn a_call_completes_48_hours_after_it_ends(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let end = booked_call(&h, &pool).await;

    let early = h.run(end + Duration::hours(24)).await;
    assert_eq!(early.completed, 0);
    assert_eq!(status(&pool, PENDING).await, "booked");

    let late = h.run(end + Duration::hours(49)).await;
    assert_eq!(late.completed, 1);
    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.status.as_str(), "completed");
    assert_eq!(c.completed_by, None);
}

#[test_db::test(seed)]
async fn a_host_cancellation_frees_the_slot_and_gives_the_days_back(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    booked_call(&h, &pool).await;
    let before = commitment(&pool, PENDING).await;
    let booked_at = before.booking.as_ref().unwrap().booked_at;
    edit_invitee(&h.cal, 1, |invitee| {
        invitee["status"] = json!("canceled");
        invitee["cancellation"] = json!({
            "canceled_by": "Dima", "canceler_type": "host",
            "created_at": calendly_time(booked_at + Duration::days(2) + Duration::hours(1)),
        });
    });

    h.run(before.created_at + Duration::days(4)).await;

    let after = commitment(&pool, PENDING).await;
    assert_eq!(after.status.as_str(), "pending");
    assert_eq!(after.booking, None);
    assert_eq!(after.deadline_at, add_local_days(before.deadline_at, 3));
    assert_eq!(
        event_kinds(&pool, PENDING).await.last().unwrap(),
        "booking_cancelled"
    );
}

#[test_db::test(seed)]
async fn a_no_show_marked_in_calendly_reopens_booking(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let end = booked_call(&h, &pool).await;
    edit_invitee(&h.cal, 1, |invitee| {
        invitee["no_show"] = json!({
            "uri": format!("{API}/invitee_no_shows/N1"),
            "created_at": calendly_time(end + Duration::hours(1)),
        });
    });

    h.run(end + Duration::hours(2)).await;

    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.status.as_str(), "pending");
    assert_eq!(c.booking, None);
    assert_eq!(c.no_show_at, Some(end + Duration::hours(1)));
    assert!(
        event_kinds(&pool, PENDING)
            .await
            .contains(&"no_show".to_string())
    );
}

#[test_db::test(seed)]
async fn a_booking_made_outside_the_widget_is_found(pool: MySqlPool) {
    only_pending(&pool).await;
    let h = harness(&pool, ON).await;
    let (created, _) = clock(&pool).await;
    let start = created + Duration::days(6);
    add_call(&h.cal, 9, "manager@onboarding.test", tag(PENDING), start);
    // The same person's other booking, not ours, comes first.
    add_call(
        &h.cal,
        8,
        "manager@onboarding.test",
        json!({}),
        start - Duration::days(1),
    );
    {
        let mut cal = h.cal.lock().unwrap();
        let events = vec![
            cal.resources["/scheduled_events/EVT-8"].clone(),
            cal.resources["/scheduled_events/EVT-9"].clone(),
        ];
        cal.by_email
            .insert("manager@onboarding.test".into(), events);
    }

    h.run(created + Duration::days(1)).await;

    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.status.as_str(), "booked");
    let booking = c.booking.unwrap();
    assert_eq!(
        booking.invitee_uri,
        format!("{API}/scheduled_events/EVT-9/invitees/INV-9")
    );
    assert_eq!(booking.start_at, start);
}

/// `requested_at`.
async fn request_cancellation(pool: &MySqlPool, requested_at: NaiveDateTime) {
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS billing_cancellation_requests (
           id BIGINT AUTO_INCREMENT PRIMARY KEY, company_id INT NOT NULL, requested_by INT NOT NULL,
           reason VARCHAR(1000) NULL, effective_at DATETIME NOT NULL,
           status ENUM('scheduled','withdrawn','completed') NOT NULL,
           requested_at DATETIME NOT NULL, withdrawn_at DATETIME NULL,
           KEY idx_cancel_company (company_id, status)
         )",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO billing_cancellation_requests
           (company_id, requested_by, effective_at, status, requested_at)
         VALUES (20400, 20400, ?, 'scheduled', ?)",
    )
    .bind(requested_at + Duration::days(90))
    .bind(requested_at)
    .execute(pool)
    .await
    .unwrap();
}

/// Company 20400's requests become `status` (`withdrawn` at `at`, or `completed`).
async fn settle_cancellation(pool: &MySqlPool, status: &str, at: NaiveDateTime) {
    sqlx::query(
        "UPDATE billing_cancellation_requests
            SET status = ?, withdrawn_at = IF(? = 'withdrawn', ?, NULL)
          WHERE company_id = 20400",
    )
    .bind(status)
    .bind(status)
    .bind(at)
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn a_scheduled_cancellation_suspends_and_its_late_withdrawal_brings_the_notice(
    pool: MySqlPool,
) {
    only_pending(&pool).await;
    let h = harness(&pool, ON).await;
    let (created, deadline) = clock(&pool).await;
    request_cancellation(&pool, created + Duration::days(1)).await;

    h.run(created + Duration::days(2)).await;
    assert_eq!(status(&pool, PENDING).await, "suspended");

    settle_cancellation(&pool, "withdrawn", deadline).await;
    let resumed = h.run(deadline + Duration::hours(1)).await;

    assert_eq!(resumed.notices, 1);
    assert_eq!(status(&pool, PENDING).await, "notice_sent");
    let kinds = event_kinds(&pool, PENDING).await;
    assert!(kinds.ends_with(&[
        "suspended".to_string(),
        "resumed".to_string(),
        "notice_sent".to_string()
    ]));
}

#[test_db::test(seed)]
async fn the_subscription_ending_voids_the_commitment(pool: MySqlPool) {
    only_pending(&pool).await;
    sqlx::query(
        "UPDATE company_billing SET subscription_status = 'canceled' WHERE company_id = 20400",
    )
    .execute(&pool)
    .await
    .unwrap();
    let h = harness(&pool, ON).await;
    let (created, _) = clock(&pool).await;
    h.run(created + Duration::days(1)).await;
    assert_eq!(status(&pool, PENDING).await, "void");
}

#[test_db::test(seed)]
async fn a_missed_call_is_never_booked_or_completed_again(pool: MySqlPool) {
    only_pending(&pool).await;
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(&pool)
        .await
        .unwrap();
    let h = harness(&pool, ON).await;
    let start = api::billing::now() - Duration::minutes(30);
    let uri = add_call(&h.cal, 1, "manager@onboarding.test", tag(PENDING), start);
    h.book(OWEN, &uri).await.assert_status_ok();
    h.server
        .put("/v1/superuser/companies/20400/onboarding-commitment")
        .with_session(sessions::ADMIN)
        .json(&json!({ "action": "no_show" }))
        .await
        .assert_status_ok();
    assert!(commitment(&pool, PENDING).await.no_show_at.is_some());

    let again = h.book(OWEN, &uri).await;
    again.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        again.json::<Value>()["error"],
        "This call was marked as missed. Please book a new time."
    );

    // Calendly still lists the call as active: the search outside the widget
    // finds it, past its 48 hours.
    {
        let mut cal = h.cal.lock().unwrap();
        let event = cal.resources["/scheduled_events/EVT-1"].clone();
        cal.by_email
            .insert("manager@onboarding.test".into(), vec![event]);
    }
    let run = h.run(start + Duration::hours(50)).await;
    assert_eq!(run.completed, 0);
    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.status.as_str(), "pending");
    assert_eq!(c.booking, None);
    assert_eq!(
        event_kinds(&pool, PENDING).await,
        ["created", "booked", "no_show"]
    );

    let later = add_call(
        &h.cal,
        2,
        "manager@onboarding.test",
        tag(PENDING),
        start + Duration::days(3),
    );
    let booked = h.book(OWEN, &later).await;
    booked.assert_status_ok();
    assert_eq!(booked.json::<Value>()["commitment"]["status"], "booked");
}

// ---- the advance payment

/// Company 20400's trial end (its first charge) as the seed stored it.
async fn trial_end(pool: &MySqlPool) -> NaiveDateTime {
    sqlx::query_scalar("SELECT trial_end_at FROM company_billing WHERE company_id = 20400")
        .fetch_one(pool)
        .await
        .unwrap()
}

impl Harness {
    fn items(&self) -> Vec<Value> {
        self.stripe.lock().unwrap().invoice_items.clone()
    }

    fn credits(&self) -> Vec<Value> {
        self.stripe.lock().unwrap().balance_transactions.clone()
    }

    fn calls(&self, method: &str, path: &str) -> usize {
        self.stripe.lock().unwrap().calls(method, path).len()
    }

    /// Stripe renews the subscription: the pending items go on `invoice_id`.
    fn renew(&self, invoice_id: &str, status: &str) -> Value {
        self.stripe
            .lock()
            .unwrap()
            .sweep_items(SUBSCRIPTION, invoice_id, status)
    }

    async fn superuser(&self, action: &str) -> axum_test::TestResponse {
        self.server
            .put("/v1/superuser/companies/20400/onboarding-commitment")
            .with_session(sessions::ADMIN)
            .json(&json!({ "action": action }))
            .await
    }
}

async fn make_superuser(pool: &MySqlPool) {
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(pool)
        .await
        .unwrap();
}

/// The notice an hour after the deadline; the trial end.
async fn noticed(h: &Harness, pool: &MySqlPool) -> NaiveDateTime {
    only_pending(pool).await;
    let (_, deadline) = clock(pool).await;
    assert_eq!(h.run(deadline + Duration::hours(1)).await.notices, 1);
    trial_end(pool).await
}

/// The notice, then the item two days before the trial end.
async fn scheduled(h: &Harness, pool: &MySqlPool) -> NaiveDateTime {
    let charge_at = noticed(h, pool).await;
    assert_eq!(h.run(charge_at - Duration::days(2)).await.advance_items, 1);
    assert_eq!(status(pool, PENDING).await, "advance_scheduled");
    charge_at
}

fn paid_event(id: &str, invoice: &Value) -> Value {
    json!({ "id": id, "object": "event", "type": "invoice.paid", "data": { "object": invoice } })
}

#[test_db::test(seed)]
async fn the_notice_fixes_a_month_of_the_live_items_on_the_trial_end_and_tells_staff(
    pool: MySqlPool,
) {
    let h = harness(&pool, ON).await;
    // An extra user on the live subscription: one month is base + users.
    {
        let mut fake = h.stripe.lock().unwrap();
        let mut extra = fake.item("price_extra_user", 2, SUBSCRIPTION);
        extra["current_period_end"] =
            fake.subscriptions[SUBSCRIPTION]["items"]["data"][0]["current_period_end"].clone();
        fake.subscriptions.get_mut(SUBSCRIPTION).unwrap()["items"]["data"]
            .as_array_mut()
            .unwrap()
            .push(extra);
    }
    let charge_at = noticed(&h, &pool).await;
    let c = commitment(&pool, PENDING).await;
    let terms = c.terms.unwrap();
    assert_eq!(terms.advance_cents, 36_000);
    assert_eq!(terms.charge_at, charge_at);
    assert_eq!(
        terms.prepaid_from,
        charge_at
            .checked_add_months(chrono::Months::new(1))
            .unwrap()
    );
    assert_eq!(
        terms.prepaid_until,
        charge_at
            .checked_add_months(chrono::Months::new(2))
            .unwrap()
    );
    assert_eq!(
        h.subjects_to("manager@onboarding.test"),
        [format!(
            "Action needed: your charge on {} will include a $360.00 advance payment",
            api::billing::reminder_emails::format_local_date(charge_at)
        )]
    );
    let staff = h.subjects_to(ALERT_EMAIL);
    assert_eq!(staff, ["Onboarding advance notice sent: Onboarding Stone"]);
    let body = h.bodies_to(ALERT_EMAIL).join("\n");
    assert!(body.contains("(id 20400)") && body.contains("$360.00"));
    assert!(!body.contains("manager@onboarding.test"));
    // Nothing reaches Stripe before the last three days.
    h.run(charge_at - Duration::days(4)).await;
    assert!(h.items().is_empty());
}

#[test_db::test(seed)]
async fn a_trial_end_under_seven_days_after_the_notice_moves_the_advance_a_period_on(
    pool: MySqlPool,
) {
    only_pending(&pool).await;
    let (_, deadline) = clock(&pool).await;
    let soon = (deadline + Duration::days(3)).and_utc().timestamp();
    sqlx::query(
        "UPDATE company_billing SET trial_end_at = FROM_UNIXTIME(?) WHERE company_id = 20400",
    )
    .bind(soon)
    .execute(&pool)
    .await
    .unwrap();
    let h = harness(&pool, ON).await;
    h.run(deadline + Duration::hours(1)).await;
    let terms = commitment(&pool, PENDING).await.terms.unwrap();
    let trial_end = chrono::DateTime::from_timestamp(soon, 0)
        .unwrap()
        .naive_utc();
    assert_eq!(
        terms.charge_at,
        trial_end
            .checked_add_months(chrono::Months::new(1))
            .unwrap()
    );
}

#[test_db::test(seed)]
async fn the_item_is_added_once_in_the_last_three_days_keyed_on_the_subscription(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let charge_at = scheduled(&h, &pool).await;
    let items = h.items();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["subscription"], SUBSCRIPTION);
    assert_eq!(item["customer"], CUSTOMER);
    assert_eq!(item["amount"], 30_000);
    assert_eq!(item["currency"], "usd");
    // Priced like the catalog: on the base product, tax exclusive.
    let sent = h.stripe.lock().unwrap().calls("POST", "/v1/invoiceitems")[0].clone();
    assert_eq!(
        sent.field("price_data[product]").as_deref(),
        Some("prod_base")
    );
    assert_eq!(
        sent.field("price_data[unit_amount]").as_deref(),
        Some("30000")
    );
    assert_eq!(sent.field("price_data[currency]").as_deref(), Some("usd"));
    assert_eq!(
        sent.field("price_data[tax_behavior]").as_deref(),
        Some("exclusive")
    );
    assert_eq!(sent.field("amount"), None);
    assert_eq!(item["metadata"]["kind"], "onboarding_advance");
    assert_eq!(item["metadata"]["company_id"], "20400");
    assert!(
        item["description"]
            .as_str()
            .unwrap()
            .starts_with("Advance payment for ")
    );
    let (_, deadline) = clock(&pool).await;
    let key = format!(
        "onboarding-advance:20400:{}",
        api::onboarding_call::advance::deadline_key(deadline)
    );
    assert_eq!(
        h.stripe.lock().unwrap().keys("POST", "/v1/invoiceitems"),
        [key]
    );
    // The next run only reads it.
    h.run(charge_at - Duration::hours(1)).await;
    assert_eq!(h.calls("POST", "/v1/invoiceitems"), 1);
    assert_eq!(h.items().len(), 1);
    assert_eq!(status(&pool, PENDING).await, "advance_scheduled");
}

#[test_db::test(seed)]
async fn a_lost_save_finds_the_item_again_instead_of_adding_a_second(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let charge_at = noticed(&h, &pool).await;
    h.run(charge_at - Duration::days(2)).await;
    // As if the save after Stripe's answer was lost.
    sqlx::query(
        "UPDATE company_onboarding_commitments SET status = 'notice_sent', advance_item_id = NULL
          WHERE company_id = 20400",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM job_runs WHERE job_key = 'onboarding-commitment:20400'")
        .execute(&pool)
        .await
        .unwrap();
    h.run(charge_at - Duration::days(2) + Duration::hours(1))
        .await;
    assert_eq!(h.calls("POST", "/v1/invoiceitems"), 1);
    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.advance_item_id.as_deref(), h.items()[0]["id"].as_str());
}

#[test_db::test(seed)]
async fn a_paid_invoice_credits_the_month_once_and_replays_change_nothing(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let charge_at = scheduled(&h, &pool).await;
    // The renewal charges the plan's month and the next one in advance:
    // $600 on a $300 plan.
    let invoice = h.renew("in_advance_1", "paid");
    assert_eq!(invoice["amount_due"], 60_000);
    post_event(&h.server, &paid_event("evt_adv_1", &invoice))
        .await
        .assert_status_ok();
    assert_eq!(status(&pool, PENDING).await, "advance_paid");
    let credits = h.credits();
    assert_eq!(credits.len(), 1);
    assert_eq!(credits[0]["amount"], -30_000);
    assert_eq!(credits[0]["customer"], CUSTOMER);
    assert_eq!(credits[0]["metadata"]["kind"], "onboarding_advance_credit");
    assert_eq!(credits[0]["metadata"]["invoice"], "in_advance_1");
    assert_eq!(
        h.stripe.lock().unwrap().keys(
            "POST",
            "/v1/customers/cus_seed_onboarding/balance_transactions"
        ),
        ["onboarding-advance:20400:in_advance_1:credit"]
    );
    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.stripe_invoice_id.as_deref(), Some("in_advance_1"));
    assert_eq!(c.advance_credit_id.as_deref(), credits[0]["id"].as_str());

    // A redelivery, another event for it and the daily run: no second credit.
    post_event(&h.server, &paid_event("evt_adv_1", &invoice))
        .await
        .assert_status_ok();
    post_event(&h.server, &paid_event("evt_adv_2", &invoice))
        .await
        .assert_status_ok();
    let run = h.run(charge_at + Duration::days(1)).await;
    assert_eq!(run.companies, 0);
    assert_eq!(h.credits().len(), 1);
    // The credit pays the following renewal in full.
    let next = h.renew("in_next", "paid");
    assert_eq!(next["total"], 30_000);
    assert_eq!(next["amount_due"], 0);
}

#[test_db::test(seed)]
async fn with_automatic_tax_the_credit_returns_the_advance_line_with_its_tax(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    scheduled(&h, &pool).await;
    h.stripe.lock().unwrap().line_tax_percent = 7;
    let invoice = h.renew("in_advance_1", "paid");
    assert_eq!(invoice["amount_due"], 2 * (30_000 + 2_100));
    post_event(&h.server, &paid_event("evt_adv_1", &invoice))
        .await
        .assert_status_ok();
    assert_eq!(status(&pool, PENDING).await, "advance_paid");
    let credits = h.credits();
    assert_eq!(credits.len(), 1);
    // The month's tax was paid with the advance: the credit returns it too.
    assert_eq!(credits[0]["amount"], -32_100);
}

#[test_db::test(seed)]
async fn without_the_webhook_the_daily_run_credits_once_even_after_a_lost_save(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let charge_at = scheduled(&h, &pool).await;
    h.renew("in_advance_1", "paid");
    let run = h.run(charge_at + Duration::hours(2)).await;
    assert_eq!(run.credits, 1);
    assert_eq!(status(&pool, PENDING).await, "advance_paid");
    // As if the credit's save was lost: the credit is found, not made again.
    sqlx::query(
        "UPDATE company_onboarding_commitments
            SET status = 'advance_scheduled', advance_credit_id = NULL WHERE company_id = 20400",
    )
    .execute(&pool)
    .await
    .unwrap();
    h.run(charge_at + Duration::days(1) + Duration::hours(3))
        .await;
    assert_eq!(status(&pool, PENDING).await, "advance_paid");
    assert_eq!(h.credits().len(), 1);
    assert_eq!(
        h.calls(
            "POST",
            "/v1/customers/cus_seed_onboarding/balance_transactions"
        ),
        1
    );
}

#[test_db::test(seed)]
async fn a_voided_invoice_with_the_advance_voids_it_and_credits_nothing(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let charge_at = scheduled(&h, &pool).await;
    h.renew("in_advance_1", "void");
    h.run(charge_at + Duration::hours(2)).await;
    assert_eq!(status(&pool, PENDING).await, "void");
    assert!(h.credits().is_empty());
    assert_eq!(commitment(&pool, PENDING).await.advance_item_id, None);
}

#[test_db::test(seed)]
async fn completing_waiving_or_holding_while_pending_deletes_the_item(pool: MySqlPool) {
    make_superuser(&pool).await;
    for (action, after) in [
        ("complete", "completed"),
        ("waive", "waived"),
        ("hold", "notice_sent"),
    ] {
        sqlx::query("DELETE FROM job_runs")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE company_onboarding_commitments
                SET status = 'pending', notice_sent_at = NULL, notice_cycle_at = NULL,
                    advance_cents = NULL, charge_at = NULL, prepaid_from_at = NULL,
                    prepaid_until_at = NULL, advance_item_id = NULL, review_hold_at = NULL,
                    review_hold_by = NULL
              WHERE company_id = 20400",
        )
        .execute(&pool)
        .await
        .unwrap();
        let h = harness(&pool, ON).await;
        scheduled(&h, &pool).await;
        assert_eq!(h.items().len(), 1, "{action}");
        let res = h.superuser(action).await;
        res.assert_status_ok();
        assert_eq!(status(&pool, PENDING).await, after, "{action}");
        assert!(h.items().is_empty(), "{action}: the item must be gone");
        assert_eq!(commitment(&pool, PENDING).await.advance_item_id, None);
        // Stripe's renewal now charges the plan's month only.
        assert_eq!(h.renew("in_after", "paid")["amount_due"], 30_000);
    }
}

#[test_db::test(seed)]
async fn a_call_booked_after_the_item_to_end_before_the_charge_deletes_it_at_once(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    let charge_at = scheduled(&h, &pool).await;
    assert_eq!(h.items().len(), 1);
    // Booked in the widget: the call ends the day before the charge.
    let uri = add_call(
        &h.cal,
        3,
        "manager@onboarding.test",
        tag(PENDING),
        charge_at - Duration::days(1),
    );
    h.book(OWEN, &uri).await.assert_status_ok();
    // No daily run needed: the item is out of Stripe before it can be swept.
    assert!(h.items().is_empty());
    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.status.as_str(), "notice_sent");
    assert_eq!(c.advance_item_id, None);
    // Booked, the advance taken back (`call_booked`), the item deleted.
    let kinds = event_kinds(&pool, PENDING).await;
    assert_eq!(
        kinds[kinds.len() - 3..],
        ["booked", "advance_withdrawn", "advance_withdrawn"]
    );
    assert_eq!(h.renew("in_after", "paid")["amount_due"], 30_000);
    // Nor does the next run add one back while that call is booked.
    assert_eq!(
        h.run(charge_at - Duration::hours(12)).await.advance_items,
        0
    );
    assert_eq!(h.calls("POST", "/v1/invoiceitems"), 1);
}

#[test_db::test(seed)]
async fn waiving_an_advance_on_a_finalized_invoice_is_refused_and_the_invoice_recorded(
    pool: MySqlPool,
) {
    make_superuser(&pool).await;
    let h = harness(&pool, ON).await;
    scheduled(&h, &pool).await;
    // Stripe swept and finalized the renewal before any run recorded it.
    h.renew("in_advance_1", "open");
    for action in ["waive", "complete", "hold"] {
        let res = h.superuser(action).await;
        res.assert_status(StatusCode::CONFLICT);
        let body: Value = res.json();
        assert!(
            body.to_string()
                .contains("already on a subscription invoice"),
            "{action}: {body}"
        );
    }
    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.status.as_str(), "advance_scheduled");
    assert_eq!(c.stripe_invoice_id.as_deref(), Some("in_advance_1"));
    assert_eq!(h.items().len(), 1);
    let view: Value = h
        .server
        .get("/v1/superuser/companies/20400/onboarding-commitment")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(view["commitment"]["allowed_actions"], json!([]));
}

#[test_db::test(seed)]
async fn waiving_an_advance_on_a_draft_invoice_deletes_it(pool: MySqlPool) {
    make_superuser(&pool).await;
    let h = harness(&pool, ON).await;
    scheduled(&h, &pool).await;
    h.renew("in_draft", "draft");
    h.superuser("waive").await.assert_status_ok();
    assert_eq!(status(&pool, PENDING).await, "waived");
    assert!(h.items().is_empty());
    assert_eq!(commitment(&pool, PENDING).await.advance_item_id, None);
}

/// Stripe ends the subscription at once (dunning, a superuser).
fn end_subscription(h: &Harness) {
    h.stripe
        .lock()
        .unwrap()
        .subscriptions
        .get_mut(SUBSCRIPTION)
        .unwrap()["status"] = json!("canceled");
}

#[test_db::test(seed)]
async fn a_subscription_ending_with_the_advance_unpaid_credits_its_line_off_the_invoice(
    pool: MySqlPool,
) {
    let h = harness(&pool, ON).await;
    let charge_at = scheduled(&h, &pool).await;
    h.stripe.lock().unwrap().line_tax_percent = 7;
    let invoice = h.renew("in_advance_1", "open");
    assert_eq!(invoice["amount_due"], 2 * 32_100);
    end_subscription(&h);

    h.run(charge_at + Duration::hours(2)).await;

    let notes = h.stripe.lock().unwrap().credit_notes.clone();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["invoice"], "in_advance_1");
    assert_eq!(notes[0]["total"], 32_100);
    assert_eq!(
        notes[0]["metadata"]["kind"],
        "onboarding_advance_credit_note"
    );
    assert_eq!(
        h.stripe.lock().unwrap().keys("POST", "/v1/credit_notes"),
        ["onboarding-advance:20400:in_advance_1:credit-note"]
    );
    let open = h.stripe.lock().unwrap().invoices[0].clone();
    assert_eq!(open["amount_due"], 32_100);
    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.status.as_str(), "void");
    assert_eq!(c.advance_item_id, None);
    assert!(h.credits().is_empty());
    assert_eq!(
        h.subjects_to(ALERT_EMAIL).last().map(String::as_str),
        Some("Onboarding advance credited off: Onboarding Stone")
    );
    // Done once: later runs leave it alone.
    h.run(charge_at + Duration::days(1) + Duration::hours(3))
        .await;
    assert_eq!(h.calls("POST", "/v1/credit_notes"), 1);
}

#[test_db::test(seed)]
async fn the_subscription_ended_webhook_credits_the_unpaid_advance_off_at_once(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    scheduled(&h, &pool).await;
    h.renew("in_advance_1", "open");
    end_subscription(&h);
    let subscription = h.stripe.lock().unwrap().subscriptions[SUBSCRIPTION].clone();

    post_event(
        &h.server,
        &json!({ "id": "evt_sub_end", "object": "event",
            "type": "customer.subscription.deleted", "data": { "object": subscription } }),
    )
    .await
    .assert_status_ok();

    // No daily run needed.
    assert_eq!(h.calls("POST", "/v1/credit_notes"), 1);
    assert_eq!(commitment(&pool, PENDING).await.status.as_str(), "void");
}

#[test_db::test(seed)]
async fn an_advance_paid_after_the_subscription_ended_is_credited_and_staff_review_it(
    pool: MySqlPool,
) {
    let h = harness(&pool, ON).await;
    scheduled(&h, &pool).await;
    let invoice = h.renew("in_advance_1", "paid");
    end_subscription(&h);
    post_event(&h.server, &paid_event("evt_adv_1", &invoice))
        .await
        .assert_status_ok();
    assert_eq!(status(&pool, PENDING).await, "advance_paid");
    assert_eq!(h.credits().len(), 1);
    assert_eq!(h.calls("POST", "/v1/credit_notes"), 0);
    assert_eq!(
        h.subjects_to(ALERT_EMAIL).last().map(String::as_str),
        Some("Onboarding advance paid after the subscription ended: Onboarding Stone")
    );
    let body = h.bodies_to(ALERT_EMAIL).last().cloned().unwrap();
    assert!(body.contains("Nothing was refunded"));
}

#[test_db::test(seed)]
async fn the_credit_finds_the_advance_line_past_the_first_ten_lines(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    scheduled(&h, &pool).await;
    // Eleven subscription lines come before the advance's.
    {
        let mut fake = h.stripe.lock().unwrap();
        fake.line_tax_percent = 7;
        for _ in 0..10 {
            let extra = fake.item("price_extra_user", 1, SUBSCRIPTION);
            fake.subscriptions.get_mut(SUBSCRIPTION).unwrap()["items"]["data"]
                .as_array_mut()
                .unwrap()
                .push(extra);
        }
    }
    let invoice = h.renew("in_advance_1", "paid");
    assert_eq!(invoice["lines"]["data"].as_array().unwrap().len(), 10);
    assert_eq!(invoice["lines"]["has_more"], true);
    post_event(&h.server, &paid_event("evt_adv_1", &invoice))
        .await
        .assert_status_ok();
    assert_eq!(status(&pool, PENDING).await, "advance_paid");
    // The advance line with its tax, read from the second page.
    assert_eq!(h.credits()[0]["amount"], -32_100);
}

#[test_db::test(seed)]
async fn a_release_under_seven_days_before_the_charge_sends_a_new_notice_a_period_later(
    pool: MySqlPool,
) {
    make_superuser(&pool).await;
    let h = harness(&pool, ON).await;
    let charge_at = noticed(&h, &pool).await;
    h.superuser("hold").await.assert_status_ok();
    // Held: no item in the window.
    h.run(charge_at - Duration::days(2)).await;
    assert!(h.items().is_empty());
    // The release (at the real clock) comes 3 days before the charge.
    sqlx::query(
        "UPDATE company_onboarding_commitments SET charge_at = UTC_TIMESTAMP() + INTERVAL 3 DAY
          WHERE company_id = 20400",
    )
    .execute(&pool)
    .await
    .unwrap();
    let released: Value = h.superuser("release").await.json();
    assert_eq!(released["commitment"]["status"], "pending");
    assert_eq!(released["events"][0]["kind"], "review_released");
    assert_eq!(released["events"][0]["detail"]["new_notice"], true);
    // The notice the run sends names the next eligible charge.
    h.run(charge_at - Duration::days(1)).await;
    let terms = commitment(&pool, PENDING).await.terms.unwrap();
    assert_eq!(
        terms.charge_at,
        charge_at
            .checked_add_months(chrono::Months::new(1))
            .unwrap()
    );
    assert_eq!(h.notices_to("manager@onboarding.test").len(), 2);
}

#[test_db::test(seed)]
async fn a_cancellation_request_while_pending_deletes_the_item_and_nothing_is_charged(
    pool: MySqlPool,
) {
    let h = harness(&pool, ON).await;
    scheduled(&h, &pool).await;
    h.server
        .post("/v1/billing/cancellation-request")
        .with_session(OWEN)
        .json(&json!({}))
        .await
        .assert_status(StatusCode::CREATED);
    assert_eq!(status(&pool, PENDING).await, "suspended");
    assert!(h.items().is_empty());
    let subscription = h.stripe.lock().unwrap().subscriptions[SUBSCRIPTION].clone();
    assert_eq!(subscription["cancel_at_period_end"], true);
    assert_eq!(h.renew("in_after", "paid")["amount_due"], 30_000);
}

#[test_db::test(seed)]
async fn a_cancellation_after_the_paid_advance_ends_with_the_prepaid_month(pool: MySqlPool) {
    let h = harness(&pool, ON).await;
    scheduled(&h, &pool).await;
    let invoice = h.renew("in_advance_1", "paid");
    post_event(&h.server, &paid_event("evt_adv_1", &invoice))
        .await
        .assert_status_ok();
    let terms = commitment(&pool, PENDING).await.terms.unwrap();
    // While the credit is unused (its renewal is ahead), the month it paid
    // for is kept.
    let until = commitment(&pool, PENDING)
        .await
        .terms
        .unwrap()
        .prepaid_until;
    assert!(until > terms.charge_at);
    let res = h
        .server
        .post("/v1/billing/cancellation-request")
        .with_session(OWEN)
        .json(&json!({}))
        .await;
    res.assert_status(StatusCode::CREATED);
    let body: Value = res.json();
    assert_eq!(body["effective_at"], json!(until.and_utc()));
    let calls = h
        .stripe
        .lock()
        .unwrap()
        .calls("POST", "/v1/subscriptions/sub_seed_onboarding");
    let last = calls.last().unwrap();
    assert_eq!(
        last.field("cancel_at"),
        Some(until.and_utc().timestamp().to_string())
    );
    assert_eq!(last.field("proration_behavior").as_deref(), Some("none"));
    assert_eq!(last.field("cancel_at_period_end"), None);
    // Withdrawn: the set end is removed.
    h.server
        .delete("/v1/billing/cancellation-request")
        .with_session(OWEN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let subscription = h.stripe.lock().unwrap().subscriptions[SUBSCRIPTION].clone();
    assert_eq!(subscription["cancel_at"], Value::Null);
}

#[test_db::test(seed)]
async fn a_cancellation_scheduled_when_the_credit_is_granted_moves_to_the_prepaid_month_end(
    pool: MySqlPool,
) {
    let h = harness(&pool, ON).await;
    let charge_at = scheduled(&h, &pool).await;
    // Swept into the renewal invoice before the request, paid after it.
    let invoice = h.renew("in_advance_1", "open");
    h.server
        .post("/v1/billing/cancellation-request")
        .with_session(OWEN)
        .json(&json!({}))
        .await
        .assert_status(StatusCode::CREATED);
    // On an invoice already: nothing to take back.
    assert_eq!(status(&pool, PENDING).await, "advance_scheduled");
    let until = commitment(&pool, PENDING)
        .await
        .terms
        .unwrap()
        .prepaid_until;
    {
        let mut fake = h.stripe.lock().unwrap();
        fake.invoices
            .iter_mut()
            .for_each(|i| i["status"] = json!("paid"));
    }
    let mut paid = invoice;
    paid["status"] = json!("paid");
    post_event(&h.server, &paid_event("evt_adv_1", &paid))
        .await
        .assert_status_ok();
    assert_eq!(status(&pool, PENDING).await, "advance_paid");
    let effective: NaiveDateTime = sqlx::query_scalar(
        "SELECT effective_at FROM billing_cancellation_requests
          WHERE company_id = 20400 AND status = 'scheduled'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(effective, until);
    let subscription = h.stripe.lock().unwrap().subscriptions[SUBSCRIPTION].clone();
    assert_eq!(subscription["cancel_at"], until.and_utc().timestamp());
    assert!(charge_at < until);
    // The Managers hear the new end date, once (a replay sends nothing).
    let moved = format!(
        "Your Granite Manager subscription now ends on {}",
        api::billing::reminder_emails::format_local_date(until)
    );
    let count = |h: &Harness| {
        h.subjects_to("manager@onboarding.test")
            .into_iter()
            .filter(|subject| *subject == moved)
            .count()
    };
    assert_eq!(count(&h), 1);
    let body = h.bodies_to("manager@onboarding.test").join("\n");
    assert!(body.contains(&format!(
        "now takes effect on {}",
        api::billing::reminder_emails::format_local_date(until)
    )));
    post_event(&h.server, &paid_event("evt_adv_2", &paid))
        .await
        .assert_status_ok();
    assert_eq!(count(&h), 1);
}

#[test_db::test(seed)]
async fn without_a_running_subscription_at_the_deadline_it_is_void_and_never_noticed(
    pool: MySqlPool,
) {
    only_pending(&pool).await;
    sqlx::query(
        "UPDATE company_billing SET stripe_subscription_id = NULL, subscription_status = 'incomplete'
          WHERE company_id = 20400",
    )
    .execute(&pool)
    .await
    .unwrap();
    let h = harness(&pool, ON).await;
    let (_, deadline) = clock(&pool).await;
    let run = h.run(deadline + Duration::hours(1)).await;
    assert_eq!(run.notices, 0);
    assert_eq!(status(&pool, PENDING).await, "void");
    assert!(h.notices_to("manager@onboarding.test").is_empty());
    assert!(h.items().is_empty());
}

#[test_db::test(seed)]
async fn with_charging_off_the_advance_is_skipped_for_good(pool: MySqlPool) {
    only_pending(&pool).await;
    let off = harness(
        &pool,
        Options {
            calendly: true,
            charging: false,
        },
    )
    .await;
    let (_, deadline) = clock(&pool).await;
    off.run(deadline + Duration::hours(1)).await;
    assert_eq!(status(&pool, PENDING).await, "advance_skipped");
    assert!(off.subjects_to("manager@onboarding.test").is_empty());

    let on = harness(&pool, ON).await;
    let later = on.run(deadline + Duration::days(60)).await;
    assert_eq!(later.companies, 0);
    assert_eq!(status(&pool, PENDING).await, "advance_skipped");
    assert!(on.items().is_empty());
}

#[test_db::test(seed)]
async fn the_superuser_view_carries_the_allowed_actions_and_the_extended_deadline(pool: MySqlPool) {
    make_superuser(&pool).await;
    let h = harness(&pool, ON).await;
    let (_, deadline) = clock(&pool).await;
    let body: Value = h
        .server
        .get("/v1/superuser/companies/20400/onboarding-commitment")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let c = &body["commitment"];
    assert_eq!(
        c["allowed_actions"],
        json!(["extend", "waive", "complete", "hold"])
    );
    assert_eq!(
        c["extension_deadline_at"],
        api::billing::iso(add_local_days(deadline, 14))
    );
    let held: Value = h.superuser("hold").await.json();
    assert_eq!(
        held["commitment"]["allowed_actions"],
        json!(["extend", "waive", "complete", "release"])
    );
    assert!(held["commitment"]["review_hold_at"].is_string());
    assert_eq!(held["commitment"]["review_hold_by"], 102);
    let done: Value = h.superuser("complete").await.json();
    assert_eq!(done["commitment"]["allowed_actions"], json!([]));
    assert_eq!(done["commitment"]["extension_deadline_at"], Value::Null);
}

#[test_db::test(seed)]
async fn a_completed_cancellation_request_never_resumes_the_advance(pool: MySqlPool) {
    only_pending(&pool).await;
    let h = harness(&pool, ON).await;
    let (created, deadline) = clock(&pool).await;
    request_cancellation(&pool, created + Duration::days(1)).await;
    h.run(created + Duration::days(2)).await;
    assert_eq!(status(&pool, PENDING).await, "suspended");
    settle_cancellation(&pool, "completed", deadline).await;
    for now in [deadline + Duration::hours(1), deadline + Duration::days(9)] {
        let run = h.run(now).await;
        assert_eq!((run.notices, run.advance_items), (0, 0));
        assert_eq!(status(&pool, PENDING).await, "suspended");
    }
    assert!(h.mailbox.lock().unwrap().is_empty());
    assert!(h.items().is_empty());
}

#[test_db::test(seed)]
async fn a_real_call_after_the_missed_one_counts_even_before_the_no_show_mark(pool: MySqlPool) {
    only_pending(&pool).await;
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(&pool)
        .await
        .unwrap();
    let h = harness(&pool, ON).await;
    let start = api::billing::now() - Duration::minutes(30);
    let uri = add_call(&h.cal, 1, "manager@onboarding.test", tag(PENDING), start);
    h.book(OWEN, &uri).await.assert_status_ok();
    let missed_start = commitment(&pool, PENDING).await.booking.unwrap().start_at;
    h.server
        .put("/v1/superuser/companies/20400/onboarding-commitment")
        .with_session(sessions::ADMIN)
        .json(&json!({ "action": "no_show" }))
        .await
        .assert_status_ok();
    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.no_show_call_start_at, Some(missed_start));
    assert!(c.no_show_at.unwrap() > missed_start + Duration::minutes(10));

    // Starts after the missed call, before the no-show was recorded.
    let between = add_call(
        &h.cal,
        2,
        "manager@onboarding.test",
        tag(PENDING),
        start + Duration::minutes(10),
    );
    let booked = h.book(OWEN, &between).await;
    booked.assert_status_ok();
    assert_eq!(booked.json::<Value>()["commitment"]["status"], "booked");
}

#[test_db::test(seed)]
async fn provisioning_stores_the_end_of_the_day_the_consent_named(pool: MySqlPool) {
    sqlx::query("DELETE FROM company_onboarding_commitment_events WHERE company_id = 20400")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM company_onboarding_commitments WHERE company_id = 20400")
        .execute(&pool)
        .await
        .unwrap();
    // 23:30 EST on 2027-02-12: the consent said "by March 14, 2027".
    let accepted_at =
        NaiveDateTime::parse_from_str("2027-02-13 04:30:00", "%Y-%m-%d %H:%M:%S").unwrap();
    let mut conn = pool.acquire().await.unwrap();
    api::signup::provision::create_onboarding_commitment(&mut conn, PENDING, 60_000, accepted_at)
        .await
        .unwrap();
    drop(conn);

    let c = commitment(&pool, PENDING).await;
    let march_14 = end_of_local_day(chrono::NaiveDate::from_ymd_opt(2027, 3, 14).unwrap());
    assert_eq!(c.deadline_at, march_14);
    assert_eq!(c.original_deadline_at, march_14);
    assert_eq!(c.deadline_at, signup_deadline_at(accepted_at));
    assert_eq!(c.deadline_at.to_string(), "2027-03-15 03:59:59");
}
