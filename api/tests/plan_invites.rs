//! Move to plan: a superuser invites a legacy company's Managers, a Manager
//! accepts on the payments app, and only then does the company follow the
//! plan. One invite never makes two subscriptions, and a legacy
//! subscription ends exactly where the new plan starts.

mod common;
#[path = "support/fake_stripe.rs"]
mod fake_stripe;
#[path = "support/fake_webhooks.rs"]
mod fake_webhooks;

use api::app;
use api::billing::catalog::signup_day;
use api::billing::invites::{local_day, token_hash};
use api::billing::stripe::signature_header;
use api::onboarding_call::commitment::end_of_local_day;
use api::signup::SignupConfig;
use api::state::AppState;
use axum::http::{Method, StatusCode};
use axum_test::{TestResponse, TestServer};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use common::{ALLOWED_ORIGIN, SECRET, WithSession, ids, sessions};
use fake_stripe::{FailMode, FakeStripe};
use fake_webhooks::Sink;
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

const APP_URL: &str = "https://granite-manager.com";
const JOB_KEY: &str = "job-secret";
const WEBHOOK_SECRET: &str = "whsec_plan_invites";
const STAFF_USER: i32 = 103;
const STAFF_EMAIL: &str = "dima@granite.test";
const INVITE_SUBJECT: &str = "Your Granite Manager plan is ready to review";
const ACTIVE_SUBJECT: &str = "Your Granite Manager plan is active";
/// Seed company 100, Acme Stone: legacy, no billing row.
const COMPANY: i32 = 100;
/// Ada Admin (102), made a superuser.
const SUPERUSER: &str = sessions::ADMIN;
/// Mark Manager (101, manager@acme.test), made a Manager of company 100.
const MANAGER: &str = sessions::MANAGER;
/// Rita Rep (100, rep@acme.test): a Manager only where a test makes her one.
const REP: &str = sessions::REP;

struct Env {
    pool: MySqlPool,
    stripe: fake_stripe::Shared,
    sink: fake_webhooks::Shared,
    server: TestServer,
}

async fn make_manager(pool: &MySqlPool, user_id: i32, company_id: i32) {
    sqlx::query("INSERT INTO users_positions (user_id, position_id, company_id) VALUES (?, 5, ?)")
        .bind(user_id)
        .bind(company_id)
        .execute(pool)
        .await
        .unwrap();
}

/// Today + `days` in the buyer's calendar, `YYYY-MM-DD`.
fn day_in(days: i64) -> String {
    (signup_day(Utc::now()) + Duration::days(days))
        .format("%Y-%m-%d")
        .to_string()
}

/// The token of the `/upgrade/<token>` link in an email body.
fn token_in(body: &str) -> String {
    let start = body.find("/upgrade/").expect("an upgrade link") + "/upgrade/".len();
    body[start..].chars().take_while(|c| *c != '"').collect()
}

fn instant(text: &Value) -> DateTime<Utc> {
    text.as_str().unwrap().parse().unwrap()
}

fn at(unix: i64) -> NaiveDateTime {
    DateTime::from_timestamp(unix, 0).unwrap().naive_utc()
}

/// The maintenance job stops resuming the invite's acceptance.
async fn give_up(pool: &MySqlPool, id: &str) {
    sqlx::query("UPDATE plan_invites SET gave_up_at = UTC_TIMESTAMP() WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

impl Env {
    async fn new(pool: &MySqlPool) -> Self {
        Self::with_seam(pool, true).await
    }

    /// `seam`: the test payment method is configured (else acceptance
    /// cannot add a payment method).
    async fn with_seam(pool: &MySqlPool, seam: bool) -> Self {
        sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
            .execute(pool)
            .await
            .unwrap();
        make_manager(pool, 101, COMPANY).await;
        let stripe = FakeStripe::with_catalog();
        let client = fake_stripe::client(&stripe).await;
        let sink: fake_webhooks::Shared = Arc::new(Mutex::new(Sink::default()));
        let webhooks = fake_webhooks::start(&sink).await;
        let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
            .with_stripe(client)
            .with_webhooks(webhooks)
            .with_signup(SignupConfig {
                test_payment_method: seam.then(|| Arc::from("pm_card_visa")),
                notify_user_id: Some(STAFF_USER),
                notify_email: Some(STAFF_EMAIL.into()),
                ..Default::default()
            })
            .with_app_url(APP_URL)
            .with_job_key(JOB_KEY)
            .with_stripe_webhook_secret(WEBHOOK_SECRET);
        Self {
            pool: pool.clone(),
            stripe,
            sink,
            server: TestServer::new(app(state)),
        }
    }

    async fn invite_for(&self, company_id: i32, body: &Value) -> TestResponse {
        self.server
            .post(&format!(
                "/v1/superuser/companies/{company_id}/plan-invites"
            ))
            .with_session(SUPERUSER)
            .json(body)
            .await
    }

    /// An invite for company 100 with Quotes, first charge 30 days out.
    async fn create(&self) -> Value {
        let response = self
            .invite_for(
                COMPANY,
                &json!({ "addons": ["quotes"], "first_charge_on": day_in(30), "expires_in_days": 14 }),
            )
            .await;
        response.assert_status(StatusCode::CREATED);
        response.json()
    }

    async fn create_response(&self) -> TestResponse {
        self.invite_for(
            COMPANY,
            &json!({ "addons": ["quotes"], "first_charge_on": day_in(30) }),
        )
        .await
    }

    async fn plan_move(&self, company_id: i32) -> Value {
        let response = self
            .server
            .get(&format!("/v1/superuser/companies/{company_id}/plan-move"))
            .with_session(SUPERUSER)
            .await;
        response.assert_status_ok();
        response.json()
    }

    /// The emails with `subject`.
    fn emails(&self, subject: &str) -> Vec<Value> {
        self.sink
            .lock()
            .unwrap()
            .emails
            .iter()
            .filter(|email| email["subject"] == subject)
            .cloned()
            .collect()
    }

    /// The token of the last invite emailed to `to`.
    fn token_for(&self, to: &str) -> String {
        let email = self
            .emails(INVITE_SUBJECT)
            .into_iter()
            .rfind(|email| email["to"] == json!([to]))
            .expect("an invite email");
        token_in(email["body"].as_str().unwrap())
    }

    async fn offer(&self, token: &str, session: &str) -> TestResponse {
        self.server
            .get(&format!("/v1/plan-invites/{token}"))
            .with_session(session)
            .await
    }

    /// The checkboxes as the upgrade page sends them back.
    async fn consents(&self, token: &str, session: &str) -> Value {
        let offer: Value = self.offer(token, session).await.json();
        Value::Array(
            offer["consents"]
                .as_array()
                .unwrap()
                .iter()
                .map(|consent| {
                    json!({ "kind": consent["kind"], "version": consent["version"],
                            "text_sha256": consent["sha256"] })
                })
                .collect(),
        )
    }

    async fn accept_with(&self, token: &str, session: &str, consents: &Value) -> TestResponse {
        self.server
            .post(&format!("/v1/plan-invites/{token}/acceptance"))
            .with_session(session)
            .json(&json!({ "consents": consents }))
            .await
    }

    async fn accept(&self, token: &str, session: &str) -> TestResponse {
        let consents = self.consents(token, session).await;
        self.accept_with(token, session, &consents).await
    }

    async fn shell(&self, session: &str) -> Value {
        self.server
            .get("/v1/app/shell")
            .with_session(session)
            .await
            .json()
    }

    fn calls(&self, method: &str, path: &str) -> Vec<fake_stripe::Seen> {
        self.stripe.lock().unwrap().calls(method, path)
    }

    async fn invite_row(&self, id: &str) -> (String, i64, Option<String>) {
        sqlx::query_as(
            "SELECT CAST(status AS CHAR), CAST(provision_attempts AS SIGNED), last_error
               FROM plan_invites WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn revoke(&self, id: &str) -> TestResponse {
        self.server
            .delete(&format!("/v1/superuser/companies/100/plan-invites/{id}"))
            .with_session(SUPERUSER)
            .await
    }

    async fn resend(&self, id: &str) -> TestResponse {
        self.server
            .post(&format!(
                "/v1/superuser/companies/100/plan-invites/{id}/emails"
            ))
            .with_session(SUPERUSER)
            .await
    }

    /// A legacy Stripe subscription `sub_legacy` of company 100 with
    /// `status`, its period ending at `period_end` (unix), and the billing row
    /// naming it.
    async fn bill_legacy(&self, status: &str, period_end: i64) {
        self.stripe.lock().unwrap().subscriptions.insert(
            "sub_legacy".into(),
            json!({ "id": "sub_legacy", "object": "subscription", "customer": "cus_legacy",
                    "status": status, "metadata": { "company_id": "100" },
                    "items": { "object": "list", "data": [{
                        "id": "si_legacy", "quantity": 1,
                        "price": { "id": "price_legacy", "nickname": "Pro", "unit_amount": 30000 },
                        "current_period_start": period_end - 30 * 86_400,
                        "current_period_end": period_end }] } }),
        );
        sqlx::query(
            "INSERT INTO company_billing (company_id, stripe_customer_id, stripe_subscription_id,
                                          subscription_status, payment_status, current_period_end)
             VALUES (100, 'cus_legacy', 'sub_legacy', ?, 'active', ?)",
        )
        .bind(status)
        .bind(at(period_end))
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// Moves `sub_legacy`'s period end in Stripe.
    fn move_legacy_period_end(&self, period_end: i64) {
        self.stripe
            .lock()
            .unwrap()
            .subscriptions
            .get_mut("sub_legacy")
            .unwrap()["items"]["data"][0]["current_period_end"] = json!(period_end);
    }

    /// Cancels a subscription in Stripe by hand.
    fn cancel_in_stripe(&self, id: &str) {
        self.stripe
            .lock()
            .unwrap()
            .subscriptions
            .get_mut(id)
            .unwrap()["status"] = json!("canceled");
    }

    /// The Stripe customer `cus_legacy` of company 100.
    fn legacy_customer(&self) {
        self.stripe.lock().unwrap().customers.insert(
            "cus_legacy".into(),
            json!({ "id": "cus_legacy", "object": "customer", "name": "Acme Stone",
                    "metadata": { "company_id": "100" },
                    "invoice_settings": { "default_payment_method": null } }),
        );
    }

    /// The subscription the invite stored, if any.
    async fn stored_subscription(&self, id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT stripe_subscription_id FROM plan_invites WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// Company 100's add-on rows.
    async fn addon_rows(&self) -> Vec<(String, String, Option<NaiveDateTime>)> {
        sqlx::query_as(
            "SELECT addon_key, CAST(source AS CHAR), ended_at FROM company_addons
              WHERE company_id = 100 ORDER BY 1, 2, 3",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn invite_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM plan_invites WHERE company_id = 100")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// Stripe sends event `id` of `kind` about `object`.
    async fn webhook(&self, id: &str, kind: &str, object: Value) {
        let event =
            json!({ "id": id, "object": "event", "type": kind, "data": { "object": object } });
        let payload = serde_json::to_vec(&event).unwrap();
        let header = signature_header(&payload, WEBHOOK_SECRET, Utc::now().timestamp());
        self.server
            .post("/v1/webhooks/stripe")
            .add_header("stripe-signature", header)
            .add_header("content-type", "application/json")
            .bytes(payload.into())
            .await
            .assert_status_ok();
    }

    /// The subscription `id` as Stripe has it now.
    fn subscription(&self, id: &str) -> Value {
        self.stripe.lock().unwrap().subscriptions[id].clone()
    }

    /// Company 100 is a self-serve company paying `sub_self` (base + Quotes,
    /// `status`) of the Stripe customer `cus_self`.
    async fn self_serve(&self, status: &str) {
        sqlx::query(
            "UPDATE company SET plan_source = 'subscription', billing_required = 1,
                    billing_required_at = UTC_TIMESTAMP() - INTERVAL 90 DAY
              WHERE id = 100",
        )
        .execute(&self.pool)
        .await
        .unwrap();
        {
            let mut stripe = self.stripe.lock().unwrap();
            stripe.customers.insert(
                "cus_self".into(),
                json!({ "id": "cus_self", "object": "customer", "name": "Acme Stone",
                        "metadata": { "company_id": "100" },
                        "invoice_settings": { "default_payment_method": null } }),
            );
            let items = vec![
                stripe.item("price_base", 1, "sub_self"),
                stripe.item("price_quotes", 1, "sub_self"),
            ];
            stripe.subscriptions.insert(
                "sub_self".into(),
                json!({ "id": "sub_self", "object": "subscription", "customer": "cus_self",
                        "status": status, "metadata": { "company_id": "100", "plan": "self_serve_v1" },
                        "items": { "object": "list", "data": items } }),
            );
        }
        sqlx::query(
            "INSERT INTO company_billing (company_id, stripe_customer_id, stripe_subscription_id,
                                          subscription_status, payment_status)
             VALUES (100, 'cus_self', 'sub_self', ?, 'active')",
        )
        .bind(status)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// `sub_self` was cancelled: it ended, which paused access.
    async fn end_self_serve(&self) {
        self.cancel_in_stripe("sub_self");
        sqlx::query(
            "UPDATE company_billing SET subscription_status = 'canceled', payment_status = 'suspended'
              WHERE company_id = 100",
        )
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// Company 100's onboarding commitment from its signup, in `status`.
    async fn commitment(&self, status: &str) {
        sqlx::query(
            "INSERT INTO company_onboarding_commitments
               (company_id, status, deadline_at, original_deadline_at, fee_cents, created_at, updated_at)
             VALUES (100, ?, UTC_TIMESTAMP(), UTC_TIMESTAMP(), 60000, UTC_TIMESTAMP(), UTC_TIMESTAMP())
             ON DUPLICATE KEY UPDATE status = VALUES(status)",
        )
        .bind(status)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn plan_source(&self, company_id: i32) -> String {
        sqlx::query_scalar("SELECT CAST(plan_source AS CHAR) FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

/// Company 100's subscription, status, free period end and scheduled end.
async fn billing_row(
    pool: &MySqlPool,
) -> (
    Option<String>,
    String,
    Option<NaiveDateTime>,
    Option<NaiveDateTime>,
) {
    sqlx::query_as(
        "SELECT stripe_subscription_id, subscription_status, trial_end_at, cancel_at
           FROM company_billing WHERE company_id = 100",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

fn code(response: &TestResponse) -> String {
    response.json::<Value>()["code"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test_db::test(seed)]
async fn superusers_invite_every_manager_and_only_token_hashes_are_kept(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    make_manager(&pool, 100, COMPANY).await;
    env.server
        .get("/v1/superuser/companies/100/plan-move")
        .with_session(MANAGER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let moving = env.plan_move(COMPANY).await;
    assert_eq!(moving["company_name"], "Acme Stone");
    assert_eq!(moving["plan_source"], "legacy");
    assert_eq!(ids(&moving["managers"]), [100, 101]);
    assert_eq!(moving["managers"][1]["email"], "manager@acme.test");
    assert_eq!(moving["legacy_subscription"], Value::Null);
    assert_eq!(moving["blocked"], Value::Null);
    assert_eq!(moving["catalog"]["base_cents"], 30_000);
    assert_eq!(moving["catalog"]["extra_user_cents"], 3_000);
    assert_eq!(
        moving["catalog"]["addons"][1],
        json!({ "key": "quotes", "label": "Quotes", "unit_cents": 7500 })
    );
    assert_eq!(moving["defaults"]["first_charge_on"], day_in(30));
    assert_eq!(moving["defaults"]["min_first_charge_on"], day_in(9));
    assert_eq!(moving["defaults"]["min_days_after_expiry"], 8);
    assert_eq!(moving["defaults"]["max_first_charge_on"], day_in(730));
    assert_eq!(moving["defaults"]["expires_in_days"], 14);
    assert_eq!(moving["invites"], json!([]));

    let created = env
        .invite_for(
            COMPANY,
            &json!({ "addons": ["quotes", "quotes"], "first_charge_on": day_in(30) }),
        )
        .await;
    created.assert_status(StatusCode::CREATED);
    let created: Value = created.json();
    assert_eq!(created["emailed"], 2);
    assert_eq!(created["not_emailed"], json!([]));
    let invite = &created["invite"];
    let id = invite["id"].as_str().unwrap().to_string();
    assert_eq!(invite["status"], "open");
    assert_eq!(invite["addons"], json!(["quotes"]));
    assert_eq!(invite["first_charge_on"], day_in(30));
    assert_eq!(invite["forced_by_legacy_subscription"], false);
    assert_eq!(invite["created_by_name"], "Ada Admin");
    assert_eq!(invite["accepted_at"], Value::Null);
    assert_eq!(invite["gave_up"], false);
    // 09:00 in Indianapolis on the picked day.
    let first_charge = instant(&invite["first_charge_at"]);
    assert_eq!(local_day(first_charge.naive_utc()).to_string(), day_in(30));
    assert!(matches!(
        first_charge.format("%H:%M").to_string().as_str(),
        "13:00" | "14:00"
    ));
    // Until the end of the day 14 days on, in Indianapolis.
    let expires = instant(&invite["expires_at"]).naive_utc();
    assert_eq!(local_day(expires).to_string(), day_in(14));
    assert_eq!(expires, end_of_local_day(local_day(expires)));

    // One email per Manager, each with its own link.
    let emails = env.emails(INVITE_SUBJECT);
    let mut recipients: Vec<&str> = emails
        .iter()
        .map(|e| e["to"][0].as_str().unwrap())
        .collect();
    recipients.sort_unstable();
    assert_eq!(recipients, ["manager@acme.test", "rep@acme.test"]);
    for email in &emails {
        assert_eq!(email["reply_to"], json!(["support@granite-manager.com"]));
        assert_eq!(email["transactional"], true);
        let body = email["body"].as_str().unwrap();
        assert!(body.contains("Quotes: $75.00 per month"));
        assert!(body.contains("Granite Manager has set up a plan for Acme Stone."));
        assert!(!body.contains('\u{2014}'));
    }
    let (mark, rita) = (
        env.token_for("manager@acme.test"),
        env.token_for("rep@acme.test"),
    );
    assert_ne!(mark, rita);
    let stored: Vec<(String, i32)> = sqlx::query_as(
        "SELECT token_sha256, user_id FROM plan_invite_tokens WHERE invite_id = ? ORDER BY user_id",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(stored, [(token_hash(&rita), 100), (token_hash(&mark), 101)]);
    env.offer(&mark, MANAGER).await.assert_status_ok();

    // A new invite replaces the open one: its links stop working.
    env.invite_for(
        COMPANY,
        &json!({ "addons": [], "first_charge_on": day_in(40) }),
    )
    .await
    .assert_status(StatusCode::CREATED);
    let moving = env.plan_move(COMPANY).await;
    let statuses: Vec<&str> = moving["invites"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["open", "revoked"]);
    assert_eq!(moving["invites"][1]["id"], id.as_str());
    let old = env.offer(&mark, MANAGER).await;
    old.assert_status(StatusCode::GONE);
    assert_eq!(code(&old), "invite_revoked");

    // The company list shows the live invite.
    let rows: Value = env
        .server
        .get("/v1/superuser/companies")
        .with_session(SUPERUSER)
        .await
        .json();
    let acme = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == COMPANY)
        .unwrap();
    assert_eq!(acme["plan_invite"]["status"], "open");
    assert!(acme["plan_invite"]["expires_at"].is_string());
}

#[test_db::test(seed)]
async fn invites_are_refused_when_the_request_or_the_company_is_not_ready(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let body = json!({ "addons": [], "first_charge_on": day_in(30) });
    env.server
        .post("/v1/superuser/companies/100/plan-invites")
        .with_session(MANAGER)
        .json(&body)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    env.invite_for(999_999, &body)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    for (body, expected) in [
        (
            json!({ "addons": ["fax"], "first_charge_on": day_in(30) }),
            "invalid_addon",
        ),
        (
            json!({ "addons": [], "first_charge_on": day_in(30), "expires_in_days": 0 }),
            "invalid_expiry",
        ),
        (
            json!({ "addons": [], "first_charge_on": day_in(30), "expires_in_days": 31 }),
            "invalid_expiry",
        ),
        (
            json!({ "addons": [], "first_charge_on": day_in(0) }),
            "invalid_first_charge",
        ),
        (
            json!({ "addons": [], "first_charge_on": day_in(731) }),
            "invalid_first_charge",
        ),
        (
            json!({ "addons": [], "first_charge_on": day_in(10), "expires_in_days": 14 }),
            "invalid_first_charge",
        ),
        // Expires on day 14: the first charge is at least 8 days later.
        (
            json!({ "addons": [], "first_charge_on": day_in(21), "expires_in_days": 14 }),
            "invalid_first_charge",
        ),
        (
            json!({ "addons": [], "first_charge_on": day_in(8), "expires_in_days": 1 }),
            "invalid_first_charge",
        ),
        (
            json!({ "addons": [], "first_charge_on": null }),
            "invalid_first_charge",
        ),
        (
            json!({ "addons": [], "first_charge_on": "next week" }),
            "invalid_first_charge",
        ),
    ] {
        let refused = env.invite_for(COMPANY, &body).await;
        refused.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(code(&refused), expected, "{body}");
    }
    let close = env
        .invite_for(
            COMPANY,
            &json!({ "addons": [], "first_charge_on": day_in(21), "expires_in_days": 14 }),
        )
        .await;
    assert_eq!(
        close.json::<Value>()["error"],
        "Pick a first charge date at least 8 days after the invite expires."
    );

    // Company 101 has no Manager; 20100 is on a plan already.
    let none = env.invite_for(101, &body).await;
    none.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&none), "no_managers");
    let on_plan = env.invite_for(20100, &body).await;
    on_plan.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&on_plan), "already_on_plan");
    assert_eq!(
        env.plan_move(20100).await["blocked"]["code"],
        "already_on_plan"
    );

    // An overdue legacy subscription.
    let period_end = (Utc::now() + Duration::days(20)).timestamp();
    env.stripe.lock().unwrap().subscriptions.insert(
        "sub_due".into(),
        json!({ "id": "sub_due", "object": "subscription", "customer": "cus_due", "status": "past_due",
                "metadata": { "company_id": "100" },
                "items": { "data": [{ "id": "si_due", "quantity": 1,
                    "price": { "id": "price_legacy", "unit_amount": 30000 },
                    "current_period_end": period_end }] } }),
    );
    sqlx::query(
        "INSERT INTO company_billing (company_id, stripe_customer_id, stripe_subscription_id,
                                      subscription_status, payment_status)
         VALUES (100, 'cus_due', 'sub_due', 'past_due', 'past_due')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let overdue = env.invite_for(COMPANY, &body).await;
    overdue.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&overdue), "legacy_overdue");
    assert_eq!(
        overdue.json::<Value>()["error"],
        "This company has an overdue payment. Settle it before moving them to a plan."
    );
    let moving = env.plan_move(COMPANY).await;
    assert_eq!(moving["blocked"]["code"], "legacy_overdue");
    assert_eq!(moving["legacy_subscription"]["status"], "past_due");
    let invites: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM plan_invites WHERE company_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(invites, 0);
    assert!(env.emails(INVITE_SUBJECT).is_empty());
}

#[test_db::test(seed)]
async fn resent_links_join_the_old_ones_and_a_withdrawn_invite_closes_them_all(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let first = env.token_for("manager@acme.test");
    let resent = env
        .server
        .post(&format!(
            "/v1/superuser/companies/100/plan-invites/{id}/emails"
        ))
        .with_session(SUPERUSER)
        .await;
    resent.assert_status_ok();
    assert_eq!(
        resent.json::<Value>(),
        json!({ "emailed": 1, "not_emailed": [] })
    );
    let second = env.token_for("manager@acme.test");
    assert_ne!(first, second);
    for token in [&first, &second] {
        env.offer(token, MANAGER).await.assert_status_ok();
    }

    // Scoped by company.
    env.server
        .delete(&format!("/v1/superuser/companies/101/plan-invites/{id}"))
        .with_session(SUPERUSER)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    env.server
        .post(&format!(
            "/v1/superuser/companies/101/plan-invites/{id}/emails"
        ))
        .with_session(SUPERUSER)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    env.server
        .delete(&format!("/v1/superuser/companies/100/plan-invites/{id}"))
        .with_session(SUPERUSER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    for token in [&first, &second] {
        let gone = env.offer(token, MANAGER).await;
        gone.assert_status(StatusCode::GONE);
        assert_eq!(code(&gone), "invite_revoked");
        let refused = env.accept_with(token, MANAGER, &json!([])).await;
        assert_eq!(code(&refused), "invite_revoked");
    }
    let resend = env
        .server
        .post(&format!(
            "/v1/superuser/companies/100/plan-invites/{id}/emails"
        ))
        .with_session(SUPERUSER)
        .await;
    resend.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&resend), "invite_closed");
    let again = env
        .server
        .delete(&format!("/v1/superuser/companies/100/plan-invites/{id}"))
        .with_session(SUPERUSER)
        .await;
    again.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&again), "invite_closed");
    assert_eq!(env.invite_row(&id).await.0, "revoked");
}

#[test_db::test(seed)]
async fn expired_invites_and_passed_first_charges_cannot_be_accepted(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let token = env.token_for("manager@acme.test");
    sqlx::query(
        "UPDATE plan_invites SET expires_at = UTC_TIMESTAMP() - INTERVAL 1 MINUTE WHERE id = ?",
    )
    .bind(&id)
    .execute(&pool)
    .await
    .unwrap();
    let expired = env.offer(&token, MANAGER).await;
    expired.assert_status(StatusCode::GONE);
    assert_eq!(code(&expired), "invite_expired");
    let refused = env.accept_with(&token, MANAGER, &json!([])).await;
    refused.assert_status(StatusCode::GONE);
    assert_eq!(code(&refused), "invite_expired");
    assert_eq!(
        env.plan_move(COMPANY).await["invites"][0]["status"],
        "expired"
    );

    // A new invite whose first charge is now less than 8 days away: the
    // reminder 7 days before it could not be sent.
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let token = env.token_for("manager@acme.test");
    for lead in [Duration::minutes(30), Duration::days(7)] {
        let consents = env.consents(&token, MANAGER).await;
        sqlx::query("UPDATE plan_invites SET first_charge_at = ? WHERE id = ?")
            .bind(Utc::now().naive_utc() + lead)
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let late = env.accept_with(&token, MANAGER, &consents).await;
        late.assert_status(StatusCode::CONFLICT);
        assert_eq!(code(&late), "invite_needs_new_date", "{lead}");
        assert_eq!(
            late.json::<Value>()["error"],
            "Your first charge would be less than 8 days away. Ask Granite Manager for a new invite."
        );
    }
    assert_eq!(env.invite_row(&id).await.0, "open");
    assert!(env.calls("POST", "/v1/customers").is_empty());
    assert!(env.calls("POST", "/v1/subscriptions").is_empty());
}

#[test_db::test(seed)]
async fn only_a_manager_of_the_invited_company_opens_or_accepts_it(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let token = env.token_for("manager@acme.test");
    env.server
        .get(&format!("/v1/plan-invites/{token}"))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // A rep of the company, a Manager of another company, a superuser
    // without the Manager position.
    for session in [REP, sessions::QUOTES_ONLY_MANAGER, SUPERUSER] {
        let refused = env.offer(&token, session).await;
        refused.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(code(&refused), "not_invited");
        let refused = env.accept_with(&token, session, &json!([])).await;
        refused.assert_status(StatusCode::FORBIDDEN);
    }
    assert_eq!(
        env.offer(&token, REP).await.json::<Value>()["error"],
        "Only a Manager of Acme Stone can accept this plan."
    );
    let unknown = env.offer("not-a-real-token-at-all-000000", MANAGER).await;
    unknown.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(code(&unknown), "invite_not_found");
    assert_eq!(env.invite_row(&id).await.0, "open");
    assert!(env.calls("POST", "/v1/customers").is_empty());
}

#[test_db::test(seed)]
async fn changed_consents_are_refused_before_anything_happens(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let token = env.token_for("manager@acme.test");
    let offer: Value = env.offer(&token, MANAGER).await.json();
    let kinds: Vec<&str> = offer["consents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["terms", "auto_renewal", "per_user_pricing"]);
    let consents = env.consents(&token, MANAGER).await;
    let mut stale = consents.clone();
    stale[1]["text_sha256"] = json!("0".repeat(64));
    let missing = Value::Array(consents.as_array().unwrap()[..2].to_vec());
    for sent in [stale, missing] {
        let refused = env.accept_with(&token, MANAGER, &sent).await;
        refused.assert_status(StatusCode::CONFLICT);
        assert_eq!(code(&refused), "consents_changed");
    }
    assert_eq!(env.invite_row(&id).await.0, "open");
    assert!(env.calls("POST", "/v1/customers").is_empty());
    assert!(env.calls("POST", "/v1/setup_intents").is_empty());
    let records: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM consent_records WHERE plan_invite_id = ?")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(records, 0);
}

#[test_db::test(seed)]
async fn accepting_moves_the_company_onto_the_plan_and_only_then(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let token = env.token_for("manager@acme.test");
    let billable = env.plan_move(COMPANY).await["billable_users"].clone();

    // Before: legacy, every add-on.
    let before = env.shell(MANAGER).await;
    assert_eq!(before["plan"]["source"], "legacy");
    assert_eq!(
        before["plan"]["addons"],
        json!(["signatures", "quotes", "quickbooks"])
    );

    let offer: Value = env.offer(&token, MANAGER).await.json();
    assert_eq!(offer["status"], "open");
    assert_eq!(offer["company_name"], "Acme Stone");
    assert_eq!(offer["addons"], json!(["quotes"]));
    assert_eq!(offer["quote"]["users"], billable);
    assert_eq!(offer["quote"]["first_charge_on"], day_in(30));
    assert_eq!(offer["quote"]["due_today_cents"], 0);
    assert_eq!(offer["quote"]["base_and_addons_cents"], 37_500);
    assert_eq!(offer["current_plan"], Value::Null);
    assert_eq!(offer["payment_mode"], "test");
    assert_eq!(offer["redirect_url"], format!("{APP_URL}/admin/billing"));
    assert_eq!(
        offer["first_charge_at"],
        created["invite"]["first_charge_at"]
    );

    let accepted = env.accept(&token, MANAGER).await;
    accepted.assert_status_ok();
    assert_eq!(
        accepted.json::<Value>(),
        json!({ "status": "accepted", "redirect_url": format!("{APP_URL}/admin/billing") })
    );

    // After: the plan decides.
    let after = env.shell(MANAGER).await;
    assert_eq!(after["plan"]["source"], "subscription");
    assert_eq!(after["plan"]["addons"], json!(["quotes"]));
    let (plan_source, billing_required, required_at): (String, i64, Option<NaiveDateTime>) =
        sqlx::query_as(
            "SELECT CAST(plan_source AS CHAR), CAST(billing_required AS SIGNED), billing_required_at
               FROM company WHERE id = 100",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (plan_source.as_str(), billing_required),
        ("subscription", 1)
    );
    assert!(required_at.is_some());

    // One Stripe customer, payment method and subscription, all keyed by the invite.
    let customers = env.calls("POST", "/v1/customers");
    assert_eq!(customers.len(), 1);
    assert_eq!(
        customers[0].idempotency_key.as_deref(),
        Some(format!("plan-invite:{id}:customer").as_str())
    );
    assert_eq!(
        customers[0].field("email").as_deref(),
        Some("manager@acme.test")
    );
    assert_eq!(customers[0].field("name").as_deref(), Some("Acme Stone"));
    assert_eq!(
        customers[0].field("metadata[plan_invite_id]"),
        Some(id.clone())
    );
    let setup = env.calls("POST", "/v1/setup_intents");
    assert_eq!(setup.len(), 1);
    assert_eq!(
        setup[0].idempotency_key.as_deref(),
        Some(format!("plan-invite:{id}:si").as_str())
    );
    assert_eq!(
        setup[0].field("metadata[purpose]").as_deref(),
        Some("plan_invite")
    );
    let creates = env.calls("POST", "/v1/subscriptions");
    assert_eq!(creates.len(), 1);
    let create = &creates[0];
    assert_eq!(
        create.idempotency_key.as_deref(),
        Some(format!("plan-invite:{id}:subscription").as_str())
    );
    assert_eq!(create.field("metadata[plan_invite_id]"), Some(id.clone()));
    assert_eq!(create.field("metadata[company_id]").as_deref(), Some("100"));
    assert_eq!(
        create.field("metadata[plan]").as_deref(),
        Some("self_serve_v1")
    );
    assert_eq!(create.field("metadata[signup_id]"), None);
    assert_eq!(
        create.field("items[0][price]").as_deref(),
        Some("price_base")
    );
    assert_eq!(
        create.field("items[1][price]").as_deref(),
        Some("price_quotes")
    );
    assert_eq!(create.field("items[2][price]"), None);
    let first_charge_at: NaiveDateTime =
        sqlx::query_scalar("SELECT first_charge_at FROM plan_invites WHERE id = ?")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        create.field("trial_end"),
        Some(first_charge_at.and_utc().timestamp().to_string())
    );
    assert_eq!(
        customers[0].field("metadata[company_id]").as_deref(),
        Some("100")
    );

    let (subscription, status, payment, trial_end): (
        Option<String>,
        String,
        String,
        Option<NaiveDateTime>,
    ) = sqlx::query_as(
        "SELECT stripe_subscription_id, subscription_status, payment_status, trial_end_at
           FROM company_billing WHERE company_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let stored: Option<String> =
        sqlx::query_scalar("SELECT stripe_subscription_id FROM plan_invites WHERE id = ?")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(subscription.is_some());
    assert_eq!(subscription, stored);
    assert_eq!((status.as_str(), payment.as_str()), ("trialing", "active"));
    assert_eq!(trial_end, Some(first_charge_at));
    let addons: Vec<(String, String)> = sqlx::query_as(
        "SELECT addon_key, CAST(source AS CHAR) FROM company_addons
          WHERE company_id = 100 AND ended_at IS NULL ORDER BY addon_key",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(addons, [("quotes".to_string(), "subscription".to_string())]);

    // The consents: terms (3 documents), auto_renewal, per_user_pricing.
    type ConsentRow = (String, String, Option<i32>, Option<i32>, String);
    let records: Vec<ConsentRow> = sqlx::query_as(
        "SELECT kind, CAST(source AS CHAR), company_id, user_id, CAST(amounts_and_dates AS CHAR)
           FROM consent_records WHERE plan_invite_id = ? ORDER BY id",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let kinds: Vec<&str> = records.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "terms",
            "terms",
            "terms",
            "auto_renewal",
            "per_user_pricing"
        ]
    );
    for (_, source, company, user, amounts) in &records {
        assert_eq!(
            (source.as_str(), *company, *user),
            ("staff_move", Some(100), Some(101))
        );
        let amounts: Value = serde_json::from_str(amounts).unwrap();
        assert_eq!(amounts["plan_invite_id"], id.as_str());
        assert_eq!(amounts["invited_by"], 102);
        assert_eq!(amounts["legacy_subscription_replaced"], false);
        assert_eq!(amounts["first_charge_on"], day_in(30));
        assert!(amounts.get("onboarding_fee_cents").is_none());
    }

    // The Manager and staff are told.
    let active = env.emails(ACTIVE_SUBJECT);
    assert_eq!(active.len(), 1);
    assert_eq!(active[0]["to"], json!(["manager@acme.test"]));
    let body = active[0]["body"].as_str().unwrap();
    assert!(body.contains("Acme Stone is now on the Granite Manager plan."));
    assert!(!body.contains('\u{2014}'));
    let telegrams = env.sink.lock().unwrap().telegrams.clone();
    assert_eq!(telegrams.len(), 1);
    assert!(
        telegrams[0]["message"]
            .as_str()
            .unwrap()
            .starts_with("Acme Stone accepted their plan: Quotes, first charge ")
    );

    // Accepting again is a no-op; the page shows it done; no new invite.
    env.accept(&token, MANAGER).await.assert_status_ok();
    assert_eq!(env.calls("POST", "/v1/subscriptions").len(), 1);
    let done: Value = env.offer(&token, MANAGER).await.json();
    assert_eq!(done["status"], "accepted");
    assert_eq!(done["consents"], json!([]));
    let moving = env.plan_move(COMPANY).await;
    assert_eq!(moving["plan_source"], "subscription");
    assert_eq!(moving["blocked"]["code"], "already_on_plan");
    assert_eq!(moving["invites"][0]["status"], "accepted");
    assert_eq!(moving["invites"][0]["accepted_by_name"], "Mark Manager");
    let again = env.create_response().await;
    again.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&again), "already_on_plan");
}

#[test_db::test(seed)]
async fn two_managers_accepting_at_once_make_one_subscription(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    make_manager(&pool, 100, COMPANY).await;
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let (mark, rita) = (
        env.token_for("manager@acme.test"),
        env.token_for("rep@acme.test"),
    );
    let (mark_consents, rita_consents) = (
        env.consents(&mark, MANAGER).await,
        env.consents(&rita, REP).await,
    );
    let (first, second) = tokio::join!(
        env.accept_with(&mark, MANAGER, &mark_consents),
        env.accept_with(&rita, REP, &rita_consents),
    );
    let mut retried = 0;
    for (response, token, session) in [(&first, &mark, MANAGER), (&second, &rita, REP)] {
        if response.status_code() == StatusCode::SERVICE_UNAVAILABLE {
            // Lost the race before the acceptance was recorded: nothing changed.
            assert_eq!(code(response), "acceptance_retry");
            retried += 1;
            env.accept(token, session).await.assert_status_ok();
        } else {
            response.assert_status_ok();
        }
    }
    assert!(retried < 2);
    assert_eq!(env.calls("POST", "/v1/subscriptions").len(), 1);
    assert_eq!(env.stripe.lock().unwrap().subscriptions.len(), 1);
    assert_eq!(env.invite_row(&id).await.0, "accepted");
    assert_eq!(env.plan_source(COMPANY).await, "subscription");
    let records: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM consent_records WHERE plan_invite_id = ?")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(records, 5);
    assert_eq!(env.emails(ACTIVE_SUBJECT).len(), 1);
}

#[test_db::test(seed)]
async fn a_legacy_subscription_ends_exactly_where_the_plan_starts(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let period_end = Utc::now().timestamp() + 20 * 86_400;
    {
        let mut stripe = env.stripe.lock().unwrap();
        stripe.customers.insert(
            "cus_legacy".into(),
            json!({ "id": "cus_legacy", "object": "customer", "name": "Acme Stone",
                    "metadata": { "company_id": "100" },
                    "invoice_settings": { "default_payment_method": null } }),
        );
        stripe.subscriptions.insert(
            "sub_legacy".into(),
            json!({ "id": "sub_legacy", "object": "subscription", "customer": "cus_legacy",
                    "status": "active", "metadata": { "company_id": "100" },
                    "items": { "object": "list", "data": [{
                        "id": "si_legacy", "quantity": 1,
                        "price": { "id": "price_legacy", "nickname": "Pro", "unit_amount": 30000 },
                        "current_period_start": period_end - 30 * 86_400,
                        "current_period_end": period_end }] } }),
        );
    }
    let end = DateTime::from_timestamp(period_end, 0).unwrap().naive_utc();
    sqlx::query(
        "INSERT INTO company_billing (company_id, stripe_customer_id, stripe_subscription_id,
                                      stripe_price_id, stripe_subscription_item_id,
                                      subscription_status, payment_status, current_period_end,
                                      plan_name, monthly_price_cents)
         VALUES (100, 'cus_legacy', 'sub_legacy', 'price_legacy', 'si_legacy', 'active', 'active',
                 ?, 'Pro', 30000)",
    )
    .bind(end)
    .execute(&pool)
    .await
    .unwrap();

    let moving = env.plan_move(COMPANY).await;
    let legacy = &moving["legacy_subscription"];
    assert_eq!(legacy["status"], "active");
    assert_eq!(legacy["plan_name"], "Pro");
    assert_eq!(legacy["monthly_cents"], 30_000);
    assert_eq!(
        instant(&legacy["current_period_end"]).timestamp(),
        period_end
    );
    assert_eq!(legacy["first_charge_on"], local_day(end).to_string());
    assert_eq!(
        legacy["latest_expiry_on"],
        (local_day(end) - Duration::days(8)).to_string()
    );
    assert_eq!(moving["blocked"], Value::Null);

    // The date is forced: the body's is ignored.
    let created = env
        .invite_for(
            COMPANY,
            &json!({ "addons": ["quotes"], "first_charge_on": null }),
        )
        .await;
    created.assert_status(StatusCode::CREATED);
    let created: Value = created.json();
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["invite"]["forced_by_legacy_subscription"], true);
    assert_eq!(
        instant(&created["invite"]["first_charge_at"]).timestamp(),
        period_end
    );
    // 14 days asked; the link stops at the end of the day 8 days before
    // the period end's.
    assert_eq!(
        instant(&created["invite"]["expires_at"]).naive_utc(),
        end_of_local_day(local_day(end) - Duration::days(8))
    );
    let emails = env.emails(INVITE_SUBJECT);
    let email = &emails[0];
    assert!(email["body"].as_str().unwrap().contains(&format!(
        "Your current plan ends on {}.",
        local_day(end).format("%B %-d, %Y")
    )));

    let token = env.token_for("manager@acme.test");
    let offer: Value = env.offer(&token, MANAGER).await.json();
    assert_eq!(
        offer["current_plan"],
        json!({ "name": "Pro", "monthly_cents": 30000, "ends_on": local_day(end).to_string() })
    );
    assert_eq!(instant(&offer["first_charge_at"]).timestamp(), period_end);
    env.accept(&token, MANAGER).await.assert_status_ok();

    // The existing customer is reused; the free period ends at the old period end.
    assert!(env.calls("POST", "/v1/customers").is_empty());
    let creates = env.calls("POST", "/v1/subscriptions");
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].field("customer").as_deref(), Some("cus_legacy"));
    assert_eq!(creates[0].field("trial_end"), Some(period_end.to_string()));
    let new_id: String =
        sqlx::query_scalar("SELECT stripe_subscription_id FROM plan_invites WHERE id = ?")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();

    // The old subscription ends at its period end, marked as replaced.
    let replaced = env.calls("POST", "/v1/subscriptions/sub_legacy");
    assert_eq!(replaced.len(), 1);
    assert_eq!(
        replaced[0].idempotency_key.as_deref(),
        Some(format!("plan-invite:{id}:replace-legacy").as_str())
    );
    assert_eq!(
        replaced[0].field("cancel_at_period_end").as_deref(),
        Some("true")
    );
    assert_eq!(
        replaced[0].field("metadata[replaced_by]"),
        Some(new_id.clone())
    );
    assert_eq!(
        replaced[0].field("metadata[plan_invite_id]"),
        Some(id.clone())
    );
    let old = env.stripe.lock().unwrap().subscriptions["sub_legacy"].clone();
    assert_eq!(old["cancel_at_period_end"], true);

    assert_eq!(
        billing_row(&pool).await,
        (Some(new_id.clone()), "trialing".into(), Some(end), None)
    );
    let consent: String = sqlx::query_scalar(
        "SELECT CAST(amounts_and_dates AS CHAR) FROM consent_records
          WHERE plan_invite_id = ? AND kind = 'auto_renewal'",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&consent).unwrap()["legacy_subscription_replaced"],
        true
    );

    // A late webhook of the old subscription (still paid, now cancelling)
    // does not take the company back to it.
    env.webhook("evt_old_update", "customer.subscription.updated", old)
        .await;
    assert_eq!(
        billing_row(&pool).await,
        (Some(new_id), "trialing".into(), Some(end), None)
    );
    assert_eq!(env.plan_source(COMPANY).await, "subscription");
}

#[test_db::test(seed)]
async fn the_billing_page_offers_the_open_invite_and_checkout_is_refused(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let checkout = env
        .server
        .post("/v1/billing/checkout")
        .with_session(MANAGER)
        .json(&json!({ "price_id": "price_flat" }))
        .await;
    checkout.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&checkout), "plan_invite_required");
    let none = env
        .server
        .post("/v1/billing/plan-invite/link")
        .with_session(MANAGER)
        .await;
    none.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(code(&none), "invite_not_found");
    let page: Value = env
        .server
        .get("/v1/billing")
        .with_session(MANAGER)
        .await
        .json();
    assert_eq!(page["plan_invite"], Value::Null);
    assert_eq!(page["starts_checkout"], false);

    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let page: Value = env
        .server
        .get("/v1/billing")
        .with_session(MANAGER)
        .await
        .json();
    assert_eq!(page["plan_invite"]["first_charge_on"], day_in(30));
    assert_eq!(
        instant(&page["plan_invite"]["expires_at"]),
        instant(&created["invite"]["expires_at"])
    );
    env.server
        .post("/v1/billing/plan-invite/link")
        .with_session(REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let link: Value = env
        .server
        .post("/v1/billing/plan-invite/link")
        .with_session(MANAGER)
        .await
        .json();
    let url = link["url"].as_str().unwrap();
    assert!(url.contains("/upgrade/"));
    let token = token_in(url);
    let offer: Value = env.offer(&token, MANAGER).await.json();
    assert_eq!(offer["status"], "open");
    let minted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM plan_invite_tokens WHERE invite_id = ? AND user_id = 101",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(minted, 2);

    env.server
        .delete(&format!("/v1/superuser/companies/100/plan-invites/{id}"))
        .with_session(SUPERUSER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    env.server
        .post("/v1/billing/plan-invite/link")
        .with_session(MANAGER)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn a_failed_switch_is_resumed_without_a_second_subscription(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let token = env.token_for("manager@acme.test");
    // Stripe makes the subscription but the answer is lost.
    env.stripe
        .lock()
        .unwrap()
        .fail(Method::POST, "/v1/subscriptions", FailMode::After, 1);
    let failed = env.accept(&token, MANAGER).await;
    failed.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code(&failed), "acceptance_retry");
    assert_eq!(
        failed.json::<Value>()["error"],
        "We could not finish switching your plan. Nothing was charged. Try again in a minute."
    );
    let (status, attempts, last_error) = env.invite_row(&id).await;
    assert_eq!((status.as_str(), attempts), ("accepting", 1));
    assert!(last_error.unwrap().starts_with("Stripe error"));
    // Legacy access stays until the switch completes.
    assert_eq!(env.plan_source(COMPANY).await, "legacy");
    let shell = env.shell(MANAGER).await;
    assert_eq!(shell["plan"]["source"], "legacy");
    // A new invite cannot replace it meanwhile.
    let blocked = env.create_response().await;
    blocked.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&blocked), "invite_accepting");

    // The maintenance job resumes it once it has waited.
    sqlx::query(
        "UPDATE plan_invites SET updated_at = updated_at - INTERVAL 30 MINUTE WHERE id = ?",
    )
    .bind(&id)
    .execute(&pool)
    .await
    .unwrap();
    let job = env
        .server
        .post("/v1/jobs/subscription-maintenance")
        .add_header("authorization", JOB_KEY)
        .await;
    job.assert_status_ok();
    let job: Value = job.json();
    assert_eq!(job["invites_resumed"], 1);
    assert_eq!(job["invites_accepted"], 1);
    assert_eq!(job["errors"], 0);
    assert_eq!(env.invite_row(&id).await.0, "accepted");
    assert_eq!(env.plan_source(COMPANY).await, "subscription");
    assert_eq!(env.stripe.lock().unwrap().subscriptions.len(), 1);
    assert_eq!(env.calls("POST", "/v1/subscriptions").len(), 1);

    // The Manager's retry finds it done.
    env.accept(&token, MANAGER).await.assert_status_ok();
    assert_eq!(env.stripe.lock().unwrap().subscriptions.len(), 1);
}

#[test_db::test(seed)]
async fn without_a_way_to_add_a_payment_method_nothing_changes(pool: MySqlPool) {
    // Payment collection is not live: no invite can be sent.
    let live = Env::with_seam(&pool, false).await;
    let moving = live.plan_move(COMPANY).await;
    let blocked = &moving["blocked"];
    assert_eq!(blocked["code"], "payment_unavailable");
    assert_eq!(
        blocked["message"],
        "Payment collection is not live yet, so a Manager could not accept. Invites can be sent once it is."
    );
    let refused = live.create_response().await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&refused), "payment_unavailable");
    assert_eq!(live.invite_count().await, 0);
    assert!(live.emails(INVITE_SUBJECT).is_empty());

    // An invite sent before cannot be accepted without one either.
    let env = Env::new(&pool).await;
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let token = env.token_for("manager@acme.test");
    let offer: Value = live.offer(&token, MANAGER).await.json();
    assert_eq!(offer["payment_mode"], "unavailable");
    let refused = live.accept(&token, MANAGER).await;
    refused.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code(&refused), "payment_setup_unavailable");
    assert_eq!(
        refused.json::<Value>()["error"],
        "Adding a payment method is not available yet. Try again later."
    );
    assert_eq!(env.invite_row(&id).await.0, "open");
    assert!(live.calls("POST", "/v1/customers").is_empty());
    assert!(live.calls("POST", "/v1/setup_intents").is_empty());
    assert_eq!(env.plan_source(COMPANY).await, "legacy");
}

#[test_db::test(seed)]
async fn a_company_that_owes_money_is_not_moved_and_cannot_accept(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let token = env.token_for("manager@acme.test");
    sqlx::query("INSERT INTO company_billing (company_id) VALUES (100)")
        .execute(&pool)
        .await
        .unwrap();
    // Access paused for an unpaid invoice, then a balance due.
    for debt in [
        "UPDATE company_billing SET payment_status = 'suspended' WHERE company_id = 100",
        "UPDATE company_billing SET payment_status = 'active', outstanding_balance_cents = 12000
          WHERE company_id = 100",
    ] {
        sqlx::query(debt).execute(&pool).await.unwrap();
        let moving = env.plan_move(COMPANY).await;
        assert_eq!(moving["blocked"]["code"], "legacy_overdue", "{debt}");
        let refused = env.create_response().await;
        refused.assert_status(StatusCode::CONFLICT);
        assert_eq!(code(&refused), "legacy_overdue");
        let refused = env.accept(&token, MANAGER).await;
        refused.assert_status(StatusCode::CONFLICT);
        assert_eq!(code(&refused), "legacy_overdue");
        assert_eq!(
            refused.json::<Value>()["error"],
            "This company has an overdue payment. Ask Granite Manager to settle it first."
        );
    }
    assert_eq!(env.invite_row(&id).await.0, "open");
    assert_eq!(env.invite_count().await, 1);
    assert!(env.calls("POST", "/v1/customers").is_empty());
    assert_eq!(env.plan_source(COMPANY).await, "legacy");
}

#[test_db::test(seed)]
async fn a_paused_or_incomplete_legacy_subscription_blocks_the_move(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    env.bill_legacy("active", Utc::now().timestamp() + 20 * 86_400)
        .await;
    for status in ["paused", "incomplete"] {
        env.stripe
            .lock()
            .unwrap()
            .subscriptions
            .get_mut("sub_legacy")
            .unwrap()["status"] = json!(status);
        let moving = env.plan_move(COMPANY).await;
        assert_eq!(moving["legacy_subscription"]["status"], status);
        assert_eq!(moving["blocked"]["code"], "legacy_subscription_unsupported");
        assert_eq!(
            moving["blocked"]["message"],
            format!(
                "The company's current Stripe subscription is {status}. Resolve it in Stripe before moving them to a plan."
            )
        );
        let refused = env.create_response().await;
        refused.assert_status(StatusCode::CONFLICT);
        assert_eq!(code(&refused), "legacy_subscription_unsupported");
    }
    assert_eq!(env.invite_count().await, 0);
}

#[test_db::test(seed)]
async fn a_legacy_renewal_too_close_blocks_the_move_and_its_late_acceptance(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    // Renews in 8 days: an invite would leave no 8 days before it.
    let soon = Utc::now().timestamp() + 8 * 86_400;
    env.bill_legacy("active", soon).await;
    let moving = env.plan_move(COMPANY).await;
    assert_eq!(moving["blocked"]["code"], "legacy_period_too_close");
    assert_eq!(
        moving["blocked"]["message"],
        format!(
            "The current plan renews on {}, too soon to switch. Send the invite after it renews.",
            local_day(at(soon)).format("%B %-d, %Y")
        )
    );
    let refused = env.create_response().await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&refused), "legacy_period_too_close");
    assert_eq!(env.invite_count().await, 0);

    // Renewed: the invite works until the day 8 days before the new period end's.
    let renewed = Utc::now().timestamp() + 15 * 86_400;
    env.move_legacy_period_end(renewed);
    let created = env
        .invite_for(COMPANY, &json!({ "addons": [], "expires_in_days": 30 }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let created: Value = created.json();
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let last_day = local_day(at(renewed)) - Duration::days(8);
    assert_eq!(
        instant(&created["invite"]["expires_at"]).naive_utc(),
        end_of_local_day(last_day)
    );
    assert_eq!(
        env.plan_move(COMPANY).await["legacy_subscription"]["latest_expiry_on"],
        last_day.to_string()
    );

    // The period end moved closer since: accepting would leave under 8 days.
    env.move_legacy_period_end(Utc::now().timestamp() + 5 * 86_400);
    let token = env.token_for("manager@acme.test");
    let late = env.accept(&token, MANAGER).await;
    late.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&late), "invite_needs_new_date");
    assert_eq!(env.invite_row(&id).await.0, "open");
    assert!(env.calls("POST", "/v1/subscriptions").is_empty());
}

#[test_db::test(seed)]
async fn a_given_up_acceptance_is_withdrawn_only_while_stripe_has_no_subscription(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    // Stripe never made the subscription.
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let token = env.token_for("manager@acme.test");
    env.stripe
        .lock()
        .unwrap()
        .fail(Method::POST, "/v1/subscriptions", FailMode::Before, 1);
    env.accept(&token, MANAGER)
        .await
        .assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(env.invite_row(&id).await.0, "accepting");
    let busy = env.revoke(&id).await;
    busy.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&busy), "invite_accepting");
    give_up(&pool, &id).await;
    let moving = env.plan_move(COMPANY).await;
    let listed = &moving["invites"][0];
    assert_eq!(listed["status"], "accepting");
    assert_eq!(listed["gave_up"], true);
    env.revoke(&id).await.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(env.invite_row(&id).await.0, "revoked");

    // Stripe made it, but the answer was lost.
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let token = env.token_for("manager@acme.test");
    env.stripe
        .lock()
        .unwrap()
        .fail(Method::POST, "/v1/subscriptions", FailMode::After, 1);
    env.accept(&token, MANAGER)
        .await
        .assert_status(StatusCode::SERVICE_UNAVAILABLE);
    give_up(&pool, &id).await;
    // Stripe cannot be read: nothing changes.
    env.stripe
        .lock()
        .unwrap()
        .fail(Method::GET, "/v1/subscriptions", FailMode::Before, 1);
    env.revoke(&id)
        .await
        .assert_status(StatusCode::SERVICE_UNAVAILABLE);
    let refused = env.revoke(&id).await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&refused), "invite_has_subscription");
    assert_eq!(
        refused.json::<Value>()["error"],
        "Stripe still has an active subscription for this invite. Cancel it in Stripe, then withdraw the invite."
    );
    assert_eq!(env.invite_row(&id).await.0, "accepting");
    // Left for staff: never cancelled here.
    let stripe = env.stripe.lock().unwrap();
    let made: Vec<&Value> = stripe
        .subscriptions
        .values()
        .filter(|sub| sub["metadata"]["plan_invite_id"] == id.as_str())
        .collect();
    assert_eq!(made.len(), 1);
    assert_eq!(made[0]["status"], "trialing");
}

#[test_db::test(seed)]
async fn a_given_up_acceptance_whose_stored_subscription_was_cancelled_is_withdrawn(
    pool: MySqlPool,
) {
    let env = Env::new(&pool).await;
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let token = env.token_for("manager@acme.test");
    // Stripe makes the subscription; the run that stored it stopped later.
    env.stripe
        .lock()
        .unwrap()
        .fail(Method::POST, "/v1/subscriptions", FailMode::After, 1);
    env.accept(&token, MANAGER)
        .await
        .assert_status(StatusCode::SERVICE_UNAVAILABLE);
    let made = env
        .stripe
        .lock()
        .unwrap()
        .subscriptions
        .values()
        .find(|sub| sub["metadata"]["plan_invite_id"] == id.as_str())
        .map(|sub| sub["id"].as_str().unwrap().to_string())
        .unwrap();
    sqlx::query("UPDATE plan_invites SET stripe_subscription_id = ? WHERE id = ?")
        .bind(&made)
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    give_up(&pool, &id).await;
    // Still live in Stripe: refused.
    let refused = env.revoke(&id).await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&refused), "invite_has_subscription");
    // Cancelled in Stripe, as the message says: withdrawn.
    env.cancel_in_stripe(&made);
    env.revoke(&id).await.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(env.invite_row(&id).await.0, "revoked");
    assert_eq!(env.plan_source(COMPANY).await, "legacy");
}

#[test_db::test(seed)]
async fn a_subscription_cancelled_in_stripe_stops_the_switch_before_anything_changes(
    pool: MySqlPool,
) {
    let env = Env::new(&pool).await;
    env.legacy_customer();
    env.bill_legacy("active", Utc::now().timestamp() + 20 * 86_400)
        .await;
    let created = env
        .invite_for(COMPANY, &json!({ "addons": ["quotes"] }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let id = created.json::<Value>()["invite"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let token = env.token_for("manager@acme.test");
    let addons_before = env.addon_rows().await;
    // The subscription is made and stored, then ending the legacy one fails.
    env.stripe.lock().unwrap().fail(
        Method::POST,
        "/v1/subscriptions/sub_legacy",
        FailMode::Before,
        1,
    );
    let failed = env.accept(&token, MANAGER).await;
    failed.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code(&failed), "acceptance_retry");
    let made = env.stored_subscription(&id).await.unwrap();

    // Staff cancel the new subscription in Stripe; the Manager checks again.
    env.cancel_in_stripe(&made);
    let stopped = env.accept_with(&token, MANAGER, &json!([])).await;
    stopped.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&stopped), "invite_subscription_ended");
    assert_eq!(
        stopped.json::<Value>()["error"],
        "The subscription for this plan was cancelled in Stripe, so the switch stopped. Ask Granite Manager to withdraw this invite and send a new one."
    );

    // Nothing switched: legacy plan, legacy subscription untouched, same add-ons.
    assert_eq!(env.plan_source(COMPANY).await, "legacy");
    assert_eq!(billing_row(&pool).await.0.as_deref(), Some("sub_legacy"));
    let legacy = env.stripe.lock().unwrap().subscriptions["sub_legacy"].clone();
    assert_eq!(legacy["status"], "active");
    assert_ne!(legacy["cancel_at_period_end"], true);
    assert_eq!(legacy["metadata"]["replaced_by"], Value::Null);
    assert_eq!(env.addon_rows().await, addons_before);

    // Given up: not resumed by the job, and staff can withdraw it.
    let (status, _, last_error) = env.invite_row(&id).await;
    assert_eq!(status, "accepting");
    assert_eq!(last_error.as_deref(), Some("invite_subscription_ended"));
    let gave_up: i64 = sqlx::query_scalar(
        "SELECT CAST(gave_up_at IS NOT NULL AS SIGNED) FROM plan_invites WHERE id = ?",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(gave_up, 1);
    assert_eq!(env.plan_move(COMPANY).await["invites"][0]["gave_up"], true);
    sqlx::query(
        "UPDATE plan_invites SET updated_at = updated_at - INTERVAL 30 MINUTE WHERE id = ?",
    )
    .bind(&id)
    .execute(&pool)
    .await
    .unwrap();
    let job = env
        .server
        .post("/v1/jobs/subscription-maintenance")
        .add_header("authorization", JOB_KEY)
        .await;
    job.assert_status_ok();
    assert_eq!(job.json::<Value>()["invites_resumed"], 0);
    env.revoke(&id).await.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(env.invite_row(&id).await.0, "revoked");
    assert_eq!(env.calls("POST", "/v1/subscriptions").len(), 1);
}

#[test_db::test(seed)]
async fn a_subscription_started_after_the_invite_was_sent_blocks_its_acceptance(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let billing_changed = "The company's billing changed since this invite was sent. Ask Granite Manager for a new invite.";
    // Sent while the company had no subscription; one started since.
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let token = env.token_for("manager@acme.test");
    env.bill_legacy("active", Utc::now().timestamp() + 40 * 86_400)
        .await;
    let refused = env.accept(&token, MANAGER).await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&refused), "invite_needs_new_date");
    assert_eq!(refused.json::<Value>()["error"], billing_changed);
    assert_eq!(env.invite_row(&id).await.0, "open");

    // Sent for that subscription, which has self-serve prices now.
    let created = env
        .invite_for(COMPANY, &json!({ "addons": ["quotes"] }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let id = created.json::<Value>()["invite"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let token = env.token_for("manager@acme.test");
    env.stripe
        .lock()
        .unwrap()
        .subscriptions
        .get_mut("sub_legacy")
        .unwrap()["items"]["data"][0]["price"] =
        fake_stripe::price("price_base", "granite_base_monthly_v1", 30_000);
    let refused = env.accept(&token, MANAGER).await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&refused), "invite_needs_new_date");
    assert_eq!(refused.json::<Value>()["error"], billing_changed);
    assert_eq!(env.invite_row(&id).await.0, "open");

    assert!(env.calls("POST", "/v1/customers").is_empty());
    assert!(env.calls("POST", "/v1/setup_intents").is_empty());
    assert!(env.calls("POST", "/v1/subscriptions").is_empty());
    assert_eq!(env.plan_source(COMPANY).await, "legacy");
}

#[test_db::test(seed)]
async fn an_invite_is_made_even_when_its_emails_fail(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    make_manager(&pool, 100, COMPANY).await;
    env.sink.lock().unwrap().fail_emails = 2;
    let created = env.create_response().await;
    created.assert_status(StatusCode::CREATED);
    let created: Value = created.json();
    assert_eq!(created["emailed"], 0);
    let mut missed: Vec<String> = serde_json::from_value(created["not_emailed"].clone()).unwrap();
    missed.sort_unstable();
    assert_eq!(missed, ["manager@acme.test", "rep@acme.test"]);
    assert_eq!(created["invite"]["status"], "open");
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    assert_eq!(env.invite_row(&id).await.0, "open");

    // A resend reports the address it could not reach.
    env.sink.lock().unwrap().fail_emails = 1;
    let resent = env.resend(&id).await;
    resent.assert_status_ok();
    let resent: Value = resent.json();
    assert_eq!(resent["emailed"], 1);
    assert_eq!(resent["not_emailed"].as_array().unwrap().len(), 1);
    assert_eq!(env.emails(INVITE_SUBJECT).len(), 1);
}

#[test_db::test(seed)]
async fn a_second_manager_retrying_sends_stripe_the_same_customer(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    // Rita (100) is the company's first Manager, Mark (101) the second.
    make_manager(&pool, 100, COMPANY).await;
    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    let (mark, rita) = (
        env.token_for("manager@acme.test"),
        env.token_for("rep@acme.test"),
    );
    // Stripe makes Mark's customer but the answer is lost.
    env.stripe
        .lock()
        .unwrap()
        .fail(Method::POST, "/v1/customers", FailMode::After, 1);
    let failed = env.accept(&mark, MANAGER).await;
    failed.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code(&failed), "acceptance_retry");
    assert_eq!(env.invite_row(&id).await.0, "open");
    // Rita's retry repeats the request under the same key.
    env.accept(&rita, REP).await.assert_status_ok();
    let customers = env.calls("POST", "/v1/customers");
    assert_eq!(customers.len(), 1);
    assert_eq!(
        customers[0].field("email").as_deref(),
        Some("rep@acme.test")
    );
    assert_eq!(env.stripe.lock().unwrap().customers.len(), 1);
    let creates = env.calls("POST", "/v1/subscriptions");
    assert_eq!(creates.len(), 1);
    let customer_id = env.stripe.lock().unwrap().customers.keys().next().cloned();
    assert_eq!(creates[0].field("customer"), customer_id);
    assert_eq!(env.invite_row(&id).await.0, "accepted");
}

#[test_db::test(seed)]
async fn the_seeded_dev_invite_opens_for_its_manager_only(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let lena = "aaaaaaaa-0000-4000-8000-000000020600";
    let rob = "aaaaaaaa-0000-4000-8000-000000020601";
    let offer = env.offer("sb11-dev-invite-token-0001", lena).await;
    offer.assert_status_ok();
    let offer: Value = offer.json();
    assert_eq!(offer["company_name"], "Legacy Move Stone");
    assert_eq!(offer["status"], "open");
    assert_eq!(offer["addons"], json!(["quotes"]));
    assert_eq!(offer["consents"].as_array().unwrap().len(), 3);
    env.offer("sb11-dev-invite-token-0001", rob)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn a_self_serve_company_whose_subscription_ended_is_moved_onto_a_new_plan(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    env.self_serve("active").await;
    // While its subscription runs it is on a plan already.
    let moving = env.plan_move(COMPANY).await;
    assert_eq!(moving["blocked"]["code"], "already_on_plan");
    assert_eq!(moving["legacy_subscription"], Value::Null);
    let refused = env.create_response().await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&refused), "already_on_plan");

    // Cancelled, its signup's onboarding fee not settled yet.
    env.end_self_serve().await;
    env.commitment("pending").await;
    let refused = env.create_response().await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(code(&refused), "onboarding_fee_open");
    env.commitment("void").await;
    let moving = env.plan_move(COMPANY).await;
    assert_eq!(moving["plan_source"], "subscription");
    assert_eq!(moving["blocked"], Value::Null);
    assert_eq!(moving["legacy_subscription"], Value::Null);
    assert_eq!(moving["catalog"]["base_cents"], 30_000);

    let created = env.create().await;
    let id = created["invite"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["invite"]["forced_by_legacy_subscription"], false);
    let token = env.token_for("manager@acme.test");
    env.accept(&token, MANAGER).await.assert_status_ok();
    assert_eq!(env.invite_row(&id).await.0, "accepted");
    assert_eq!(env.plan_source(COMPANY).await, "subscription");

    // Its Stripe customer is reused and nothing old is ended.
    assert!(env.calls("POST", "/v1/customers").is_empty());
    let creates = env.calls("POST", "/v1/subscriptions");
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].field("customer").as_deref(), Some("cus_self"));
    assert!(env.calls("POST", "/v1/subscriptions/sub_self").is_empty());

    // Its one billing row follows the new subscription, paid up.
    let new_id = env.stored_subscription(&id).await.unwrap();
    let first_charge_at: NaiveDateTime =
        sqlx::query_scalar("SELECT first_charge_at FROM plan_invites WHERE id = ?")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        billing_row(&pool).await,
        (Some(new_id), "trialing".into(), Some(first_charge_at), None)
    );
    let payments: Vec<String> =
        sqlx::query_scalar("SELECT payment_status FROM company_billing WHERE company_id = 100")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(payments, ["active"]);
    // A move brings no onboarding commitment.
    let commitments: Vec<String> = sqlx::query_scalar(
        "SELECT CAST(status AS CHAR) FROM company_onboarding_commitments WHERE company_id = 100",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(commitments, ["void"]);
    let consent: String = sqlx::query_scalar(
        "SELECT CAST(amounts_and_dates AS CHAR) FROM consent_records
          WHERE plan_invite_id = ? AND kind = 'auto_renewal'",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let amounts: Value = serde_json::from_str(&consent).unwrap();
    assert_eq!(amounts["legacy_subscription_replaced"], false);
    assert!(amounts.get("onboarding_fee_cents").is_none());

    // Running again: on a plan.
    assert_eq!(
        env.plan_move(COMPANY).await["blocked"]["code"],
        "already_on_plan"
    );
}

#[test_db::test(seed)]
async fn events_of_an_unaccepted_invites_subscription_leave_the_legacy_billing_alone(
    pool: MySqlPool,
) {
    let env = Env::new(&pool).await;
    env.legacy_customer();
    env.bill_legacy("active", Utc::now().timestamp() + 20 * 86_400)
        .await;
    let before = billing_row(&pool).await;
    let created = env
        .invite_for(COMPANY, &json!({ "addons": ["quotes"] }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let id = created.json::<Value>()["invite"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let token = env.token_for("manager@acme.test");
    let addons_before = env.addon_rows().await;
    // The subscription is made, then ending the legacy one fails.
    env.stripe.lock().unwrap().fail(
        Method::POST,
        "/v1/subscriptions/sub_legacy",
        FailMode::Before,
        1,
    );
    env.accept(&token, MANAGER)
        .await
        .assert_status(StatusCode::SERVICE_UNAVAILABLE);
    let made = env.stored_subscription(&id).await.unwrap();
    assert_eq!(
        env.subscription(&made)["metadata"]["plan_invite_id"],
        id.as_str()
    );
    let untouched = async || {
        assert_eq!(billing_row(&pool).await, before);
        let payment: String =
            sqlx::query_scalar("SELECT payment_status FROM company_billing WHERE company_id = 100")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(payment, "active");
        assert_eq!(env.plan_source(COMPANY).await, "legacy");
        assert_eq!(env.addon_rows().await, addons_before);
    };

    // Its created and deleted events change nothing.
    env.webhook(
        "evt_invite_created",
        "customer.subscription.created",
        env.subscription(&made),
    )
    .await;
    untouched().await;
    env.cancel_in_stripe(&made);
    env.webhook(
        "evt_invite_deleted",
        "customer.subscription.deleted",
        env.subscription(&made),
    )
    .await;
    untouched().await;

    // Given up and withdrawn; the next invite is accepted.
    let stopped = env.accept_with(&token, MANAGER, &json!([])).await;
    assert_eq!(code(&stopped), "invite_subscription_ended");
    env.revoke(&id).await.assert_status(StatusCode::NO_CONTENT);
    untouched().await;
    let created = env
        .invite_for(COMPANY, &json!({ "addons": ["quotes"] }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let id = created.json::<Value>()["invite"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let token = env.token_for("manager@acme.test");
    env.accept(&token, MANAGER).await.assert_status_ok();
    let accepted = env.stored_subscription(&id).await.unwrap();
    assert_eq!(billing_row(&pool).await.0, Some(accepted.clone()));

    // Accepted: its events apply like any subscription's.
    env.stripe
        .lock()
        .unwrap()
        .subscriptions
        .get_mut(&accepted)
        .unwrap()["status"] = json!("past_due");
    env.webhook(
        "evt_accepted_updated",
        "customer.subscription.updated",
        env.subscription(&accepted),
    )
    .await;
    let (subscription, status, ..) = billing_row(&pool).await;
    assert_eq!(
        (subscription, status.as_str()),
        (Some(accepted), "past_due")
    );
}
