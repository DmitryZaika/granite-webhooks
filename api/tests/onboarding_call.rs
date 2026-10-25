//! Onboarding with Dima: `/v1/onboarding-call`, the superuser routes, the
//! daily job (`onboarding_call::job::run_at` with fixed clocks) and the fee
//! invoice's Stripe webhook, against a fake Calendly, a fake Stripe and a
//! fake mail service. Seed ids 20400-20499 are this file's: 20400 pending
//! (Owen Manager 20400, Rita Rep 20401, Ada Admin 20402), 20401 booked (Bea
//! Manager 20403).

mod common;

use api::app;
use api::billing::stripe::{Stripe, signature_header};
use api::onboarding_call::Config;
use api::onboarding_call::calendly::{Calendly, calendly_time};
use api::onboarding_call::job::{self, OnboardingRun};
use api::onboarding_call::store;
use api::signup::SignupConfig;
use api::state::AppState;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
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
const PREFIX_NOTICE: &str = "Action needed: onboarding fee of $600.00 will be charged on ";

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

// ---- fake Stripe

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Pay {
    #[default]
    Paid,
    /// A bank debit on its way: the invoice stays open.
    Processing,
    Declined,
}

#[derive(Default)]
struct FakeStripe {
    invoices: HashMap<String, Value>,
    /// Idempotency key -> invoice id of `POST /v1/invoices`.
    keys: HashMap<String, String>,
    creates: usize,
    items: usize,
    finalizes: usize,
    pays: usize,
    pay: Pay,
    /// (path, form)
    requests: Vec<(String, String)>,
}

type SharedStripe = Arc<Mutex<FakeStripe>>;

fn form(body: &str) -> HashMap<String, String> {
    serde_html_form::from_str(body).unwrap()
}

fn invoice_or_404(stripe: &FakeStripe, id: &str) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    stripe.invoices.get(id).cloned().map(Json).ok_or((
        StatusCode::NOT_FOUND,
        Json(json!({ "error": { "message": "No such invoice" } })),
    ))
}

async fn fake_stripe(stripe: SharedStripe) -> String {
    let router = Router::new()
        .route(
            "/v1/invoices",
            get(|State(s): State<SharedStripe>, RawQuery(query): RawQuery| async move {
                let s = s.lock().unwrap();
                let customer = params(query).remove("customer").unwrap_or_default();
                let data: Vec<Value> = s
                    .invoices
                    .values()
                    .filter(|invoice| invoice["customer"] == customer.as_str())
                    .cloned()
                    .collect();
                Json(json!({ "object": "list", "data": data }))
            })
            .post(|State(s): State<SharedStripe>, headers: HeaderMap, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("/v1/invoices".into(), body.clone()));
                let key = headers
                    .get("idempotency-key")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                if let Some(id) = s.keys.get(&key).cloned() {
                    return Json(s.invoices[&id].clone());
                }
                s.creates += 1;
                let id = format!("in_fee_{}", s.creates);
                let params = form(&body);
                let invoice = json!({
                    "id": id, "object": "invoice", "customer": params["customer"],
                    "status": "draft", "attempted": false, "lines": { "data": [] },
                    "metadata": {
                        "kind": params["metadata[kind]"],
                        "company_id": params["metadata[company_id]"],
                        "deadline": params["metadata[deadline]"],
                    },
                });
                s.keys.insert(key, id.clone());
                s.invoices.insert(id, invoice.clone());
                Json(invoice)
            }),
        )
        .route(
            "/v1/invoices/{id}",
            get(|State(s): State<SharedStripe>, Path(id): Path<String>| async move {
                invoice_or_404(&s.lock().unwrap(), &id)
            }),
        )
        .route(
            "/v1/invoiceitems",
            post(|State(s): State<SharedStripe>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests.push(("/v1/invoiceitems".into(), body.clone()));
                s.items += 1;
                let params = form(&body);
                let line = json!({ "id": "il_1", "amount": params["amount"].parse::<i64>().unwrap() });
                if let Some(invoice) = s.invoices.get_mut(&params["invoice"]) {
                    invoice["lines"]["data"].as_array_mut().unwrap().push(line);
                }
                Json(json!({ "id": "ii_1", "object": "invoiceitem" }))
            }),
        )
        .route(
            "/v1/invoices/{id}/finalize",
            post(|State(s): State<SharedStripe>, Path(id): Path<String>| async move {
                let mut s = s.lock().unwrap();
                s.finalizes += 1;
                if let Some(invoice) = s.invoices.get_mut(&id) {
                    invoice["status"] = json!("open");
                }
                invoice_or_404(&s, &id)
            }),
        )
        .route(
            "/v1/invoices/{id}/pay",
            post(|State(s): State<SharedStripe>, Path(id): Path<String>| async move {
                let mut s = s.lock().unwrap();
                s.pays += 1;
                let pay = s.pay;
                let Some(invoice) = s.invoices.get_mut(&id) else {
                    return Err((StatusCode::NOT_FOUND, Json(json!({ "error": { "message": "No such invoice" } }))));
                };
                invoice["attempted"] = json!(true);
                match pay {
                    Pay::Paid => invoice["status"] = json!("paid"),
                    Pay::Processing => {}
                    Pay::Declined => {
                        return Err((
                            StatusCode::PAYMENT_REQUIRED,
                            Json(json!({ "error": { "type": "card_error", "code": "card_declined", "message": "Your card was declined." } })),
                        ));
                    }
                }
                Ok(Json(invoice.clone()))
            }),
        )
        .with_state(stripe);
    serve(router).await
}

// ---- mail and the harness

/// Each `/email/send` body as sent.
type Mailbox = Arc<Mutex<Vec<Value>>>;

struct Harness {
    server: TestServer,
    state: AppState,
    cal: SharedCal,
    stripe: SharedStripe,
    mailbox: Mailbox,
    /// Each `/telegram/notifications-notify` body.
    telegrams: Mailbox,
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
    let stripe = SharedStripe::default();
    let stripe_base = fake_stripe(Arc::clone(&stripe)).await;
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
        .with_stripe(Stripe::new(Some("sk_test_fake"), &stripe_base))
        .with_stripe_webhook_secret(WEBHOOK_SECRET)
        .with_signup(SignupConfig {
            notify_user_id: Some(STAFF_USER),
            notify_email: Some(Arc::from(STAFF_EMAIL)),
            ..SignupConfig::default()
        })
        .with_onboarding_call(Config {
            calendly,
            fee_charging: options.charging,
        });
    Harness {
        server: TestServer::new(app(state.clone())),
        state,
        cal,
        stripe,
        mailbox,
        telegrams,
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
        let mut day = first.unwrap_or_else(|| now.date());
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

    /// Subjects of the fee notices to `email`.
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
    assert_eq!(body["commitment"]["fee_cents"], 60_000);
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
    h.server
        .post("/v1/jobs/onboarding-commitments")
        .add_header("Authorization", "job-secret")
        .await
        .assert_status_ok();
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
        ["Book your Granite Manager onboarding call"]
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
    assert_eq!(c.deadline_at, deadline + Duration::days(1));
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
        deadline + Duration::days(1)
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
    assert_eq!(after.deadline_at, before.deadline_at + Duration::days(3));
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

#[test_db::test(seed)]
async fn after_the_deadline_the_notice_then_exactly_one_invoice(pool: MySqlPool) {
    only_pending(&pool).await;
    let h = harness(&pool, ON).await;
    h.stripe.lock().unwrap().pay = Pay::Processing;
    let (_, deadline) = clock(&pool).await;
    let noticed_at = deadline + Duration::hours(1);

    let first = h.run(noticed_at).await;

    assert_eq!(first.notices, 1);
    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.status.as_str(), "notice_sent");
    let charge_after = noticed_at + Duration::days(7);
    assert_eq!(c.charge_after, Some(charge_after));
    let subjects = h.subjects_to("manager@onboarding.test");
    assert_eq!(subjects.len(), 1);
    assert!(subjects[0].starts_with(PREFIX_NOTICE));
    assert!(h.subjects_to("rep@onboarding.test").is_empty());

    let charged = h.run(charge_after + Duration::hours(1)).await;
    let again = h
        .run(charge_after + Duration::days(1) + Duration::hours(2))
        .await;

    assert_eq!(charged.invoices, 1);
    assert_eq!(again.invoices, 0);
    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.status.as_str(), "fee_invoiced");
    assert_eq!(c.stripe_invoice_id.as_deref(), Some("in_fee_1"));
    let stripe = h.stripe.lock().unwrap();
    assert_eq!(
        (stripe.creates, stripe.items, stripe.finalizes, stripe.pays),
        (1, 1, 1, 1)
    );
    let created = form(
        &stripe
            .requests
            .iter()
            .find(|(path, _)| path == "/v1/invoices")
            .unwrap()
            .1,
    );
    assert_eq!(created["customer"], "cus_seed_onboarding");
    assert_eq!(created["pending_invoice_items_behavior"], "exclude");
    assert_eq!(created["auto_advance"], "false");
    assert_eq!(created["metadata[kind]"], "onboarding_fee");
    assert_eq!(created["metadata[company_id]"], "20400");
    assert_eq!(
        created["metadata[deadline]"],
        deadline.format("%Y-%m-%d").to_string()
    );
    let item = form(
        &stripe
            .requests
            .iter()
            .find(|(path, _)| path == "/v1/invoiceitems")
            .unwrap()
            .1,
    );
    assert_eq!(item["invoice"], "in_fee_1");
    assert_eq!(item["amount"], "60000");
    assert_eq!(item["currency"], "usd");
}

#[test_db::test(seed)]
async fn a_paid_fee_invoice_is_fee_paid(pool: MySqlPool) {
    only_pending(&pool).await;
    let h = harness(&pool, ON).await;
    let (_, deadline) = clock(&pool).await;
    h.run(deadline + Duration::hours(1)).await;
    h.run(deadline + Duration::days(8) + Duration::hours(2))
        .await;
    assert_eq!(status(&pool, PENDING).await, "fee_paid");
}

#[test_db::test(seed)]
async fn a_declined_fee_invoice_is_fee_failed_and_staff_are_told_once(pool: MySqlPool) {
    only_pending(&pool).await;
    let h = harness(&pool, ON).await;
    h.stripe.lock().unwrap().pay = Pay::Declined;
    let (_, deadline) = clock(&pool).await;
    h.run(deadline + Duration::hours(1)).await;
    h.run(deadline + Duration::days(8) + Duration::hours(2))
        .await;
    assert_eq!(status(&pool, PENDING).await, "fee_failed");
    assert_eq!(
        h.subjects_to(STAFF_EMAIL),
        ["Onboarding fee payment failed: Onboarding Stone"]
    );
    let body = h.bodies_to(STAFF_EMAIL).remove(0);
    assert!(body.contains("(id 20400)") && body.contains("$600.00"));
    assert!(!body.contains("onboarding.test"));
    let telegrams = h.telegrams.lock().unwrap().clone();
    assert_eq!(telegrams.len(), 1);
    assert_eq!(telegrams[0]["user_id"], STAFF_USER);
    let text = telegrams[0]["message"].as_str().unwrap();
    assert!(text.contains("Onboarding Stone (id 20400)") && text.contains("$600.00"));
    assert!(!text.contains("onboarding.test"));

    // Stripe's own failure event for the same invoice tells nobody again.
    post_event(
        &h.server,
        &fee_event("evt_9", "invoice.payment_failed", "in_fee_1"),
    )
    .await
    .assert_status_ok();
    assert_eq!(h.subjects_to(STAFF_EMAIL).len(), 1);
    assert_eq!(h.telegrams.lock().unwrap().len(), 1);
}

fn fee_event(id: &str, kind: &str, invoice_id: &str) -> Value {
    json!({ "id": id, "object": "event", "type": kind, "data": { "object": {
        "id": invoice_id, "object": "invoice", "customer": "cus_seed_onboarding",
        "status": "open", "amount_due": 60_000, "amount_paid": 0, "currency": "usd",
        "metadata": { "kind": "onboarding_fee", "company_id": "20400", "deadline": "2026-11-20" },
    }}})
}

async fn billing_snapshot(pool: &MySqlPool) -> (String, String, i32, i64) {
    let row: (String, String, i32) = sqlx::query_as(
        "SELECT subscription_status, payment_status, outstanding_balance_cents
           FROM company_billing WHERE company_id = 20400",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let invoices: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM company_billing_invoices WHERE company_id = 20400",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    (row.0, row.1, row.2, invoices)
}

async fn claimed_events(pool: &MySqlPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM stripe_webhook_events")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn fee_invoice_webhooks_move_the_commitment_and_never_billing(pool: MySqlPool) {
    sqlx::query(
        "UPDATE company_onboarding_commitments
            SET status = 'fee_invoiced', stripe_invoice_id = 'in_fee_1', version = version + 1
          WHERE company_id = 20400",
    )
    .execute(&pool)
    .await
    .unwrap();
    let h = harness(&pool, ON).await;
    let billing = billing_snapshot(&pool).await;
    let claimed_before = claimed_events(&pool).await;

    post_event(
        &h.server,
        &fee_event("evt_1", "invoice.payment_failed", "in_other"),
    )
    .await
    .assert_status_ok();
    assert_eq!(status(&pool, PENDING).await, "fee_invoiced");
    post_event(
        &h.server,
        &fee_event("evt_2", "invoice.finalized", "in_fee_1"),
    )
    .await
    .assert_status_ok();
    assert_eq!(status(&pool, PENDING).await, "fee_invoiced");

    assert!(h.subjects_to(STAFF_EMAIL).is_empty());
    post_event(
        &h.server,
        &fee_event("evt_3", "invoice.payment_failed", "in_fee_1"),
    )
    .await
    .assert_status_ok();
    assert_eq!(status(&pool, PENDING).await, "fee_failed");
    assert_eq!(
        h.subjects_to(STAFF_EMAIL),
        ["Onboarding fee payment failed: Onboarding Stone"]
    );
    post_event(
        &h.server,
        &fee_event("evt_3b", "invoice.payment_failed", "in_fee_1"),
    )
    .await
    .assert_status_ok();
    assert_eq!(h.subjects_to(STAFF_EMAIL).len(), 1);
    assert_eq!(h.telegrams.lock().unwrap().len(), 1);
    post_event(&h.server, &fee_event("evt_4", "invoice.paid", "in_fee_1"))
        .await
        .assert_status_ok();
    assert_eq!(status(&pool, PENDING).await, "fee_paid");

    assert_eq!(billing_snapshot(&pool).await, billing);
    assert_eq!(claimed_events(&pool).await, claimed_before);
    assert_eq!(
        event_kinds(&pool, PENDING).await,
        ["created", "fee_failed", "fee_paid"]
    );
}

/// `billing_cancellation_requests` (step 10 owns it; its DDL from
/// 02-architecture.md section 4) with company 20400's request, scheduled at
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
async fn with_charging_off_the_fee_is_skipped_for_good(pool: MySqlPool) {
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
    assert_eq!(status(&pool, PENDING).await, "fee_skipped");
    assert!(off.subjects_to("manager@onboarding.test").is_empty());

    let on = harness(&pool, ON).await;
    let later = on.run(deadline + Duration::days(9)).await;
    assert_eq!(later.companies, 0);
    assert_eq!(status(&pool, PENDING).await, "fee_skipped");
    assert_eq!(on.stripe.lock().unwrap().creates, 0);
    assert!(on.mailbox.lock().unwrap().is_empty());
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

#[test_db::test(seed)]
async fn a_completed_cancellation_request_never_resumes_the_fee(pool: MySqlPool) {
    only_pending(&pool).await;
    let h = harness(&pool, ON).await;
    let (created, deadline) = clock(&pool).await;
    request_cancellation(&pool, created + Duration::days(1)).await;
    h.run(created + Duration::days(2)).await;
    assert_eq!(status(&pool, PENDING).await, "suspended");

    // Step 10 completes the request when the subscription ends; the billing
    // row can lag behind it.
    settle_cancellation(&pool, "completed", deadline).await;
    for now in [deadline + Duration::hours(1), deadline + Duration::days(9)] {
        let run = h.run(now).await;
        assert_eq!((run.notices, run.invoices), (0, 0));
        assert_eq!(status(&pool, PENDING).await, "suspended");
    }
    assert!(h.mailbox.lock().unwrap().is_empty());
    assert_eq!(h.stripe.lock().unwrap().creates, 0);

    sqlx::query(
        "UPDATE company_billing SET subscription_status = 'incomplete_expired'
          WHERE company_id = 20400",
    )
    .execute(&pool)
    .await
    .unwrap();
    h.run(deadline + Duration::days(10)).await;
    assert_eq!(status(&pool, PENDING).await, "void");
}

#[test_db::test(seed)]
async fn a_cancellation_during_the_notice_suspends_and_its_withdrawal_sends_a_new_notice(
    pool: MySqlPool,
) {
    only_pending(&pool).await;
    let h = harness(&pool, ON).await;
    let (_, deadline) = clock(&pool).await;
    let noticed_at = deadline + Duration::hours(1);
    assert_eq!(h.run(noticed_at).await.notices, 1);

    // After the deadline, before the charge date the notice gave.
    request_cancellation(&pool, deadline + Duration::days(2)).await;
    h.run(deadline + Duration::days(2) + Duration::hours(2))
        .await;
    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.status.as_str(), "suspended");
    assert_eq!((c.charge_after, c.notice_cycle_at), (None, None));

    settle_cancellation(&pool, "withdrawn", deadline + Duration::days(3)).await;
    let renoticed_at = deadline + Duration::days(3) + Duration::hours(3);
    assert_eq!(h.run(renoticed_at).await.notices, 1);
    let c = commitment(&pool, PENDING).await;
    assert_eq!(c.status.as_str(), "notice_sent");
    assert_eq!(c.charge_after, Some(renoticed_at + Duration::days(7)));
    assert_eq!(c.notice_cycle_at, Some(renoticed_at));
    // The new charge date came with a new email.
    assert_eq!(h.notices_to("manager@onboarding.test").len(), 2);

    // The first notice's charge date passes without a charge.
    let run = h
        .run(noticed_at + Duration::days(7) + Duration::hours(4))
        .await;
    assert_eq!(run.invoices, 0);
    assert_eq!(status(&pool, PENDING).await, "notice_sent");
    assert_eq!(h.stripe.lock().unwrap().creates, 0);
}

#[test_db::test(seed)]
async fn a_fee_draft_is_never_finalized_or_paid_once_charging_is_off(pool: MySqlPool) {
    only_pending(&pool).await;
    let (_, deadline) = clock(&pool).await;
    let noticed_at = deadline + Duration::hours(1);
    sqlx::query(
        "UPDATE company_onboarding_commitments
            SET status = 'fee_invoiced', stripe_invoice_id = 'in_fee_draft', notice_sent_at = ?,
                notice_cycle_at = ?, charge_after = ?, version = version + 1
          WHERE company_id = 20400",
    )
    .bind(noticed_at)
    .bind(noticed_at)
    .bind(noticed_at + Duration::days(7))
    .execute(&pool)
    .await
    .unwrap();
    // A run that created and recorded the draft, then stopped.
    let draft = json!({
        "id": "in_fee_draft", "object": "invoice", "customer": "cus_seed_onboarding",
        "status": "draft", "attempted": false,
        "lines": { "data": [{ "id": "il_1", "amount": 60_000 }] },
        "metadata": {
            "kind": "onboarding_fee", "company_id": "20400",
            "deadline": deadline.format("%Y-%m-%d").to_string(),
        },
    });
    let off = harness(
        &pool,
        Options {
            calendly: true,
            charging: false,
        },
    )
    .await;
    off.stripe
        .lock()
        .unwrap()
        .invoices
        .insert("in_fee_draft".into(), draft.clone());

    let run = off.run(noticed_at + Duration::days(8)).await;

    assert_eq!(run.invoices, 0);
    assert_eq!(status(&pool, PENDING).await, "fee_invoiced");
    {
        let stripe = off.stripe.lock().unwrap();
        assert_eq!(
            (stripe.creates, stripe.items, stripe.finalizes, stripe.pays),
            (0, 0, 0, 0)
        );
        assert_eq!(stripe.invoices["in_fee_draft"]["status"], "draft");
    }

    // The same draft with charging on is finished.
    let on = harness(&pool, ON).await;
    on.stripe
        .lock()
        .unwrap()
        .invoices
        .insert("in_fee_draft".into(), draft);
    on.run(noticed_at + Duration::days(9)).await;
    assert_eq!(status(&pool, PENDING).await, "fee_paid");
    let stripe = on.stripe.lock().unwrap();
    assert_eq!((stripe.creates, stripe.finalizes, stripe.pays), (0, 1, 1));
}
