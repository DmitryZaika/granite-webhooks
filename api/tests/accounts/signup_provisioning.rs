//! Self-serve signup completion and provisioning: one signup makes one
//! company and one subscription whatever crashes, retries or races happen,
//! and `POST /v1/jobs/subscription-maintenance` resumes and expires signups.

use crate::common;
use crate::fake_stripe;
use crate::fake_webhooks;

use api::app;
use api::billing::catalog::Catalog;
use api::billing::stripe::SignupCustomer;
use api::error::ApiError;
use api::signup::consent::{self, ConsentAcceptance, PaymentMethodType};
use api::signup::maintenance;
use api::signup::provision::{self, CompletionOutcome};
use api::signup::{RequestMeta, SignupConfig, intent};
use api::state::AppState;
use axum::http::Method;
use axum_test::TestServer;
use chrono::{Duration, NaiveDateTime, SubsecRound, Utc};
use common::{ALLOWED_ORIGIN, SECRET};
use fake_stripe::{FailMode, FakeStripe};
use fake_webhooks::Sink;
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub(crate) const JOB_KEY: &str = "job-secret";
const PASSWORD: &str = "Correct-Horse-9";
const STAFF_USER: i32 = 101;
pub(crate) const STAFF_EMAIL: &str = "dima@granite.test";
pub(crate) const APP_URL: &str = "https://granite-manager.com";

/// Shared with `signup_bank`.
pub(crate) struct Env {
    pub(crate) pool: MySqlPool,
    pub(crate) state: AppState,
    pub(crate) stripe: fake_stripe::Shared,
    pub(crate) sink: fake_webhooks::Shared,
}

impl Env {
    pub(crate) async fn new(pool: &MySqlPool) -> Self {
        let stripe = FakeStripe::with_catalog();
        let client = fake_stripe::client(&stripe).await;
        let sink: fake_webhooks::Shared = Arc::new(Mutex::new(Sink::default()));
        let webhooks = fake_webhooks::start(&sink).await;
        let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
            .with_stripe(client)
            .with_webhooks(webhooks)
            .with_signup(SignupConfig {
                enabled: true,
                notify_user_id: Some(STAFF_USER),
                notify_email: Some(STAFF_EMAIL.into()),
                ..Default::default()
            })
            .with_app_url(APP_URL)
            .with_job_key(JOB_KEY);
        Self {
            pool: pool.clone(),
            state,
            stripe,
            sink,
        }
    }

    pub(crate) fn now() -> NaiveDateTime {
        Utc::now().naive_utc().trunc_subsecs(0)
    }

    /// A Stripe customer of a signup, as payment setup made it.
    pub(crate) async fn customer(&self, id: &str, email: &str) -> String {
        let customer = self
            .state
            .stripe
            .create_signup_customer(&SignupCustomer {
                signup_id: id,
                company_name: "Test Stone",
                email,
                phone: "+13175550100",
                line1: "1 Main St",
                city: "Indianapolis",
                state: "IN",
                postal_code: "46204",
            })
            .await
            .unwrap();
        customer["id"].as_str().unwrap().to_string()
    }

    /// Inserts a signup row in `status` with a Stripe customer, created
    /// `age` ago (it expires 24 hours after that).
    pub(crate) async fn intent_row(
        &self,
        email: &str,
        company: &str,
        status: &str,
        age: Duration,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let customer = self.customer(&id, email).await;
        let created = Self::now() - age;
        sqlx::query(
            "INSERT INTO signup_intents
               (id, status, email, verified_at, company, owner_name, password_hash, plan,
                stripe_customer_id, created_at, updated_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(status)
        .bind(email)
        .bind(created)
        .bind(
            json!({
                "name": company, "phone": "+13175550100", "website": null,
                "street": "1 Main St", "city": "Indianapolis", "state": "IN", "zip": "46204",
            })
            .to_string(),
        )
        .bind("Olive <Owner>")
        .bind(bcrypt::hash(PASSWORD, 4).unwrap())
        .bind(json!({ "users": 12, "addons": ["quotes", "signatures"] }).to_string())
        .bind(&customer)
        .bind(created)
        .bind(created)
        .bind(created + Duration::hours(24))
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    /// A `payment_ready` signup whose SetupIntent the browser confirmed with
    /// a card (unless `confirmed` is false).
    pub(crate) async fn ready_signup(&self, email: &str, company: &str, confirmed: bool) -> String {
        let id = self
            .intent_row(email, company, "payment_ready", Duration::minutes(5))
            .await;
        let customer = intent::load(&self.pool, &id)
            .await
            .unwrap()
            .unwrap()
            .stripe_customer_id
            .unwrap();
        let setup = self
            .state
            .stripe
            .create_signup_setup_intent(&customer, &id, 1)
            .await
            .unwrap();
        let setup_id = setup["id"].as_str().unwrap().to_string();
        if confirmed {
            self.stripe
                .lock()
                .unwrap()
                .confirm_setup_intent(&setup_id, "pm_card_visa");
        }
        sqlx::query(
            "UPDATE signup_intents SET setup_intent_id = ?, setup_attempts = 1 WHERE id = ?",
        )
        .bind(&setup_id)
        .bind(&id)
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    /// The checkboxes as the page would send them back, paying by card.
    pub(crate) async fn acceptances(&self, id: &str) -> Vec<ConsentAcceptance> {
        self.acceptances_for(id, PaymentMethodType::Card).await
    }

    /// The base checkboxes and the payment one of `method`, as the page
    /// shows them.
    pub(crate) async fn acceptances_for(
        &self,
        id: &str,
        method: PaymentMethodType,
    ) -> Vec<ConsentAcceptance> {
        let intent = intent::load(&self.pool, id).await.unwrap().unwrap();
        let catalog = Catalog::load(&self.state.stripe).await.unwrap();
        let facts = consent::facts(&catalog, &intent, Utc::now());
        consent::render(&facts, APP_URL)
            .into_iter()
            .filter(|rendered| rendered.applies_to(method))
            .map(|rendered| ConsentAcceptance {
                kind: rendered.kind,
                version: rendered.version.to_string(),
                text_sha256: rendered.sha256,
            })
            .collect()
    }

    pub(crate) async fn complete(&self, id: &str) -> Result<CompletionOutcome, ApiError> {
        let accepted = self.acceptances(id).await;
        provision::complete(&self.state, id, &accepted, &meta()).await
    }

    pub(crate) async fn job(&self) -> Value {
        let server = TestServer::new(app(self.state.clone()));
        let response = server
            .post("/v1/jobs/subscription-maintenance")
            .add_header("authorization", JOB_KEY)
            .await;
        response.assert_status_ok();
        response.json()
    }

    pub(crate) async fn intent(&self, id: &str) -> intent::Intent {
        intent::load(&self.pool, id).await.unwrap().unwrap()
    }

    pub(crate) async fn count(&self, sql: &str, bind: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(bind)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    pub(crate) async fn companies_named(&self, name: &str) -> i64 {
        self.count("SELECT COUNT(*) FROM company WHERE name = ?", name)
            .await
    }

    pub(crate) fn subscription_posts(&self) -> Vec<fake_stripe::Seen> {
        self.stripe
            .lock()
            .unwrap()
            .calls("POST", "/v1/subscriptions")
    }

    pub(crate) fn subscriptions(&self) -> usize {
        self.stripe.lock().unwrap().subscriptions.len()
    }

    /// (welcome emails to the owner, Telegram notices, staff emails)
    pub(crate) fn notices(&self, owner: &str) -> (usize, usize, usize) {
        let sink = self.sink.lock().unwrap();
        (
            sink.emails_to(owner).len(),
            sink.telegrams.len(),
            sink.emails_to(STAFF_EMAIL).len(),
        )
    }
}

pub(crate) fn meta() -> RequestMeta {
    RequestMeta {
        ip: Some("203.0.113.9".into()),
        user_agent: Some("test-agent".into()),
        locale: Some("en-US".into()),
    }
}

pub(crate) fn code(err: &ApiError) -> (u16, &'static str) {
    match err {
        ApiError::Coded(coded) => (coded.status.as_u16(), coded.code),
        other => panic!("expected a coded error, got {other:?}"),
    }
}

#[test_db::test(seed)]
async fn completion_provisions_the_company_and_subscription(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let email = "owner@happy.test";
    let id = env.ready_signup(email, "Happy Stone", true).await;

    let outcome = env.complete(&id).await.unwrap();
    assert_eq!(
        outcome.redirect_url,
        format!("{APP_URL}/employee?welcome=1")
    );
    let user_id = outcome.sign_in.expect("this request finished the signup");

    let intent = env.intent(&id).await;
    assert_eq!(intent.status, intent::SignupStatus::Completed);
    assert_eq!(intent.password_hash, "");
    assert_eq!(intent.user_id, Some(user_id));
    let company_id = intent.company_id.unwrap();
    let trial_end = intent.trial_end_at.unwrap();

    let (subdomain, plan_source, billing_required): (Option<String>, String, i64) = sqlx::query_as(
        "SELECT subdomain, CAST(plan_source AS CHAR), CAST(billing_required AS SIGNED) FROM company WHERE id = ?",
    )
    .bind(company_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(subdomain, None);
    assert_eq!(plan_source, "subscription");
    assert_eq!(billing_required, 1);

    let (owner_email, is_admin, is_employee, company): (String, i64, i64, i32) = sqlx::query_as(
        "SELECT email, CAST(is_admin AS SIGNED), CAST(is_employee AS SIGNED), company_id FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (owner_email.as_str(), is_admin, is_employee, company),
        (email, 1, 1, company_id)
    );
    let positions: Vec<i32> = sqlx::query_scalar(
        "SELECT position_id FROM users_positions WHERE user_id = ? AND company_id = ?",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(positions, vec![5]);
    let walk_ins: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE company_id = ? AND is_system = 1")
            .bind(company_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(walk_ins, 1);

    let (customer, subscription, billing_trial_end, status, payment): (
        Option<String>,
        Option<String>,
        Option<NaiveDateTime>,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT stripe_customer_id, stripe_subscription_id, trial_end_at, subscription_status, payment_status
           FROM company_billing WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(customer, intent.stripe_customer_id);
    assert_eq!(subscription, intent.stripe_subscription_id);
    assert!(subscription.is_some());
    assert_eq!(billing_trial_end, Some(trial_end));
    assert_eq!((status.as_str(), payment.as_str()), ("trialing", "active"));

    let consents: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM consent_records WHERE signup_intent_id = ? AND company_id = ? AND user_id = ?",
    )
    .bind(&id)
    .bind(company_id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    // Four checkboxes and the card one; the terms one records terms, privacy
    // and DPA apart.
    assert_eq!(consents, 7);
    let (card, mandate, sec_code): (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT payment_method_id, stripe_mandate_id, CAST(sec_code_intended AS CHAR)
               FROM consent_records WHERE signup_intent_id = ? AND kind = 'card_authorization'",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (card.as_deref(), mandate, sec_code),
        (Some("pm_card_visa"), None, None)
    );
    let ach: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM consent_records WHERE signup_intent_id = ? AND kind = 'ach_authorization'",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ach, 0);
    let documents: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT kind, document_version, document_sha256 FROM consent_records
          WHERE signup_intent_id = ? ORDER BY id",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let terms: Vec<(&str, Option<&str>)> = documents
        .iter()
        .filter(|(kind, _, _)| kind == "terms")
        .map(|(_, version, sha)| (version.as_str(), sha.as_deref()))
        .collect();
    assert_eq!(
        terms,
        [
            (consent::TERMS_VERSION, Some(consent::TERMS_SHA256)),
            (consent::PRIVACY_VERSION, Some(consent::PRIVACY_SHA256)),
            (consent::DPA_VERSION, Some(consent::DPA_SHA256)),
        ]
    );
    let amounts: Value = serde_json::from_str(
        &sqlx::query_scalar::<_, String>(
            "SELECT CAST(amounts_and_dates AS CHAR) FROM consent_records
              WHERE signup_intent_id = ? AND kind = 'onboarding_advance'",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap(),
    )
    .unwrap();
    let deadline: chrono::NaiveDate = amounts["onboarding_deadline_on"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    // The advance goes on the first charge (at least 7 days after the deadline).
    assert_eq!(
        amounts["onboarding_advance_charge_on"],
        amounts["first_charge_on"]
    );
    let charge_on: chrono::NaiveDate = amounts["onboarding_advance_charge_on"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(charge_on >= deadline + Duration::days(7));
    assert_eq!(amounts["extra_user_cents"], 3000);

    let addons: Vec<String> = sqlx::query_scalar(
        "SELECT addon_key FROM company_addons
          WHERE company_id = ? AND source = 'subscription' AND ended_at IS NULL ORDER BY addon_key",
    )
    .bind(company_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(addons, vec!["quotes", "signatures"]);

    let posts = env.subscription_posts();
    assert_eq!(posts.len(), 1);
    let post = &posts[0];
    assert_eq!(
        post.idempotency_key.as_deref(),
        Some(format!("signup:{id}:subscription").as_str())
    );
    let prices: Vec<String> = (0..)
        .map_while(|index| post.field(&format!("items[{index}][price]")))
        .collect();
    assert_eq!(
        prices,
        vec!["price_base", "price_signatures", "price_quotes"]
    );
    assert_eq!(
        post.field("trial_end"),
        Some(trial_end.and_utc().timestamp().to_string())
    );
    assert_eq!(
        post.field("metadata[company_id]"),
        Some(company_id.to_string())
    );

    assert_eq!(env.notices(email), (1, 1, 1));
    let sink = env.sink.lock().unwrap();
    let welcome = &sink.emails_to(email)[0];
    assert_eq!(welcome["subject"], "Welcome to Granite Manager");
    assert_eq!(welcome["from_address"], "noreply@granite-manager.com");
    let body = welcome["body"].as_str().unwrap();
    assert!(body.contains("Olive &lt;Owner&gt;"));
    assert!(body.contains(&format!("{APP_URL}/login")));
    assert!(!body.contains('\u{2014}'));
    let telegram = &sink.telegrams[0];
    assert_eq!(telegram["user_id"], STAFF_USER);
    let text = telegram["message"].as_str().unwrap();
    assert!(text.contains("Happy Stone") && text.contains(&company_id.to_string()));
    assert!(!text.contains(email) && !text.contains("5550100"));
    let staff = sink.emails_to(STAFF_EMAIL)[0]["body"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(staff.contains(email) && staff.contains("+13175550100"));
}

#[test_db::test(seed)]
async fn a_lost_subscription_answer_is_resumed_without_a_second_subscription(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let id = env
        .ready_signup("owner@lost.test", "Lost Answer Stone", true)
        .await;
    env.stripe
        .lock()
        .unwrap()
        .fail(Method::POST, "/v1/subscriptions", FailMode::After, 1);

    let err = env.complete(&id).await.unwrap_err();
    assert_eq!(code(&err), (503, "provisioning_retry"));
    let intent = env.intent(&id).await;
    assert_eq!(intent.status, intent::SignupStatus::Provisioning);
    assert!(intent.company_id.is_some());
    assert_eq!(intent.stripe_subscription_id, None);
    assert_eq!(intent.provision_attempts, 1);
    assert!(intent.last_error.is_some());

    let outcome = env.complete(&id).await.unwrap();
    assert!(outcome.sign_in.is_some());
    let done = env.intent(&id).await;
    assert_eq!(done.status, intent::SignupStatus::Completed);
    assert_eq!(done.company_id, intent.company_id);
    assert_eq!(env.companies_named("Lost Answer Stone").await, 1);
    assert_eq!(env.subscriptions(), 1);
    assert_eq!(env.subscription_posts().len(), 1);
    let replayed = env
        .stripe
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|seen| seen.path == "/v1/subscriptions" && seen.replayed)
        .count();
    assert_eq!(replayed, 1);
    assert_eq!(
        env.count(
            "SELECT COUNT(*) FROM consent_records WHERE signup_intent_id = ?",
            &id
        )
        .await,
        7
    );
    assert_eq!(env.notices("owner@lost.test"), (1, 1, 1));
}

#[test_db::test(seed)]
async fn a_retry_after_the_free_period_passed_adopts_the_dated_attempts_subscription(
    pool: MySqlPool,
) {
    let env = Env::new(&pool).await;
    let id = env
        .ready_signup("owner@late.test", "Late Retry Stone", true)
        .await;
    // The dated request made the subscription; its answer was lost.
    env.stripe
        .lock()
        .unwrap()
        .fail(Method::POST, "/v1/subscriptions", FailMode::After, 1);
    env.complete(&id).await.unwrap_err();
    // The free period ends before the retry: it bills `now`.
    sqlx::query(
        "UPDATE signup_intents SET trial_end_at = UTC_TIMESTAMP() - INTERVAL 1 HOUR WHERE id = ?",
    )
    .bind(&id)
    .execute(&pool)
    .await
    .unwrap();

    env.complete(&id).await.unwrap();
    assert_eq!(
        env.intent(&id).await.status,
        intent::SignupStatus::Completed
    );
    assert_eq!(env.subscriptions(), 1);
    let keys: Vec<Option<String>> = env
        .subscription_posts()
        .iter()
        .map(|seen| seen.idempotency_key.clone())
        .collect();
    assert_eq!(keys, [Some(format!("signup:{id}:subscription"))]);
}

#[test_db::test(seed)]
async fn a_resume_cancels_a_second_subscription_of_the_signup(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let id = env
        .ready_signup("owner@race.test", "Race Stone", true)
        .await;
    env.stripe
        .lock()
        .unwrap()
        .fail(Method::POST, "/v1/subscriptions", FailMode::After, 1);
    env.complete(&id).await.unwrap_err();
    let lost = env
        .stripe
        .lock()
        .unwrap()
        .subscriptions
        .keys()
        .next()
        .cloned()
        .unwrap();
    // A racing run stored another subscription of the signup first.
    let customer = env.intent(&id).await.stripe_customer_id.unwrap();
    env.stripe.lock().unwrap().subscriptions.insert(
        "sub_raced".into(),
        json!({ "id": "sub_raced", "object": "subscription", "customer": customer,
                "status": "trialing", "metadata": { "signup_id": id.as_str() },
                "items": { "object": "list", "data": [] } }),
    );
    sqlx::query("UPDATE signup_intents SET stripe_subscription_id = 'sub_raced' WHERE id = ?")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();

    env.complete(&id).await.unwrap();
    let stripe = env.stripe.lock().unwrap();
    assert_eq!(stripe.subscriptions[&lost]["status"], "canceled");
    assert_eq!(stripe.subscriptions["sub_raced"]["status"], "trialing");
}

#[test_db::test(seed)]
async fn the_job_finishes_a_signup_stopped_by_a_stripe_outage(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let email = "owner@outage.test";
    let id = env.ready_signup(email, "Outage Stone", true).await;
    env.stripe
        .lock()
        .unwrap()
        .fail(Method::POST, "/v1/subscriptions", FailMode::Before, 1);
    let err = env.complete(&id).await.unwrap_err();
    assert_eq!(code(&err), (503, "provisioning_retry"));

    // One failed run: the job waits 20 minutes before the next.
    let early = env.job().await;
    assert_eq!(early["resumed"], 0);
    sqlx::query(
        "UPDATE signup_intents SET updated_at = updated_at - INTERVAL 15 MINUTE WHERE id = ?",
    )
    .bind(&id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(env.job().await["resumed"], 0);

    sqlx::query(
        "UPDATE signup_intents SET updated_at = updated_at - INTERVAL 6 MINUTE WHERE id = ?",
    )
    .bind(&id)
    .execute(&pool)
    .await
    .unwrap();
    let run = env.job().await;
    assert_eq!(run["resumed"], 1);
    assert_eq!(run["completed"], 1);
    assert_eq!(run["errors"], 0);
    assert_eq!(run["processed"], 1);
    assert_eq!(
        env.intent(&id).await.status,
        intent::SignupStatus::Completed
    );

    let later = env.complete(&id).await.unwrap();
    assert_eq!(later.sign_in, None);
    assert_eq!(env.subscriptions(), 1);
    assert_eq!(env.companies_named("Outage Stone").await, 1);
    assert_eq!(env.notices(email), (1, 1, 1));
}

#[test_db::test(seed)]
async fn two_completions_at_once_make_one_company(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let email = "owner@race.test";
    let id = env.ready_signup(email, "Race Stone", true).await;
    let accepted = env.acceptances(&id).await;
    let meta = meta();
    let (first, second) = tokio::join!(
        provision::complete(&env.state, &id, &accepted, &meta),
        provision::complete(&env.state, &id, &accepted, &meta),
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    let signed_in = [first.sign_in, second.sign_in]
        .iter()
        .filter(|user| user.is_some())
        .count();
    assert_eq!(signed_in, 1);
    assert_eq!(env.companies_named("Race Stone").await, 1);
    assert_eq!(env.subscriptions(), 1);
    assert_eq!(
        env.count(
            "SELECT COUNT(*) FROM consent_records WHERE signup_intent_id = ?",
            &id
        )
        .await,
        7
    );
    assert_eq!(env.notices(email), (1, 1, 1));

    // A completion retried after success changes nothing.
    let again = env.complete(&id).await.unwrap();
    assert_eq!(again.sign_in, None);
    assert_eq!(env.subscriptions(), 1);
    assert_eq!(env.notices(email), (1, 1, 1));
}

#[test_db::test(seed)]
async fn the_job_and_a_user_retry_racing_finish_once(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let email = "owner@jobrace.test";
    let id = env.ready_signup(email, "Job Race Stone", true).await;
    env.stripe
        .lock()
        .unwrap()
        .fail(Method::POST, "/v1/subscriptions", FailMode::After, 1);
    assert!(env.complete(&id).await.is_err());
    sqlx::query(
        "UPDATE signup_intents SET updated_at = updated_at - INTERVAL 11 MINUTE WHERE id = ?",
    )
    .bind(&id)
    .execute(&pool)
    .await
    .unwrap();
    let accepted = env.acceptances(&id).await;
    let meta = meta();
    let (job, user) = tokio::join!(
        env.job(),
        provision::complete(&env.state, &id, &accepted, &meta),
    );
    let user = user.unwrap();
    let completed_by_job = job["completed"].as_u64().unwrap();
    assert_eq!(completed_by_job + u64::from(user.sign_in.is_some()), 1);
    assert_eq!(
        env.intent(&id).await.status,
        intent::SignupStatus::Completed
    );
    assert_eq!(env.companies_named("Job Race Stone").await, 1);
    assert_eq!(env.subscriptions(), 1);
    assert_eq!(env.notices(email), (1, 1, 1));
}

#[test_db::test(seed)]
async fn changed_consents_and_unconfirmed_payment_are_refused(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let id = env
        .ready_signup("owner@stale.test", "Stale Stone", true)
        .await;
    let mut accepted = env.acceptances(&id).await;
    accepted[1].text_sha256 = "0".repeat(64);
    let err = provision::complete(&env.state, &id, &accepted, &meta())
        .await
        .unwrap_err();
    assert_eq!(code(&err), (409, "consents_changed"));
    let intent = env.intent(&id).await;
    assert_eq!(intent.status, intent::SignupStatus::PaymentReady);
    assert_eq!(intent.accepted_at, None);
    assert_eq!(env.companies_named("Stale Stone").await, 0);
    assert_eq!(
        env.count(
            "SELECT COUNT(*) FROM consent_records WHERE signup_intent_id = ?",
            &id
        )
        .await,
        0
    );
    assert!(env.subscription_posts().is_empty());

    let unconfirmed = env
        .ready_signup("owner@unconfirmed.test", "Unconfirmed Stone", false)
        .await;
    let err = env.complete(&unconfirmed).await.unwrap_err();
    assert_eq!(code(&err), (409, "payment_not_ready"));
    assert_eq!(
        env.intent(&unconfirmed).await.status,
        intent::SignupStatus::PaymentReady
    );

    let err = provision::complete(&env.state, &uuid::Uuid::new_v4().to_string(), &[], &meta())
        .await
        .unwrap_err();
    assert_eq!(code(&err), (404, "signup_expired"));
}

#[test_db::test(seed)]
async fn an_email_taken_before_provisioning_refuses_the_signup(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let id = env
        .ready_signup("owner@taken.test", "Taken Stone", true)
        .await;
    let customer = env.intent(&id).await.stripe_customer_id.unwrap();
    // Taken meanwhile, in another case, by a user since deleted.
    sqlx::query(
        "INSERT INTO users (email, password, name, is_employee, is_admin, is_superuser, is_deleted, company_id)
         VALUES ('Owner@Taken.test', '', 'Someone', 1, 0, 0, 1, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let err = env.complete(&id).await.unwrap_err();
    assert_eq!(code(&err), (409, "email_taken"));
    let intent = env.intent(&id).await;
    assert_eq!(intent.status, intent::SignupStatus::Expired);
    assert_eq!(intent.password_hash, "");
    assert_eq!(intent.last_error.as_deref(), Some("email_taken"));
    assert_eq!(intent.company_id, None);
    assert!(!env.stripe.lock().unwrap().customers.contains_key(&customer));
    assert_eq!(env.companies_named("Taken Stone").await, 0);
    assert!(env.subscription_posts().is_empty());

    let err = env.complete(&id).await.unwrap_err();
    assert_eq!(code(&err), (410, "signup_expired"));
}

#[test_db::test(seed)]
async fn the_job_expires_abandoned_signups_and_reconciles_seats_daily(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    // A subscription company the seat reconcile covers.
    let done = env
        .ready_signup("owner@seats.test", "Seats Stone", true)
        .await;
    env.complete(&done).await.unwrap();
    // A legacy company with a Stripe subscription: never touched.
    sqlx::query(
        "INSERT INTO company_billing (company_id, stripe_customer_id, stripe_subscription_id, subscription_status, payment_status)
         VALUES (100, 'cus_legacy', 'sub_legacy', 'active', 'active')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let abandoned = env
        .intent_row(
            "owner@abandoned.test",
            "Abandoned Stone",
            "verified",
            Duration::hours(25),
        )
        .await;
    let abandoned_customer = env.intent(&abandoned).await.stripe_customer_id.unwrap();
    let fresh = env
        .intent_row(
            "owner@fresh.test",
            "Fresh Stone",
            "verified",
            Duration::hours(2),
        )
        .await;
    let bank_kept = env
        .intent_row(
            "owner@bank1.test",
            "Bank Kept",
            "awaiting_bank_verification",
            Duration::days(10),
        )
        .await;
    // Waiting on its bank account for weeks: its company exists, never expired.
    let bank_old = env
        .intent_row(
            "owner@bank2.test",
            "Bank Old",
            "awaiting_bank_verification",
            Duration::days(30),
        )
        .await;

    let first = env.job().await;
    assert_eq!(first["expired"], 1);
    assert_eq!(first["processed"], 1);
    assert_eq!(first["resumed"], 0);
    assert_eq!(first["errors"], 0);
    // This signup plus the seeded subscription companies 20400 and 20500 (they
    // need no extra-user item).
    assert_eq!(first["seat_companies"], 3);
    assert_eq!(first["seat_errors"], 0);

    let gone = env.intent(&abandoned).await;
    assert_eq!(gone.status, intent::SignupStatus::Expired);
    assert_eq!(gone.password_hash, "");
    assert_eq!(gone.code_hash, None);
    assert!(
        !env.stripe
            .lock()
            .unwrap()
            .customers
            .contains_key(&abandoned_customer)
    );
    assert_eq!(
        env.intent(&fresh).await.status,
        intent::SignupStatus::Verified
    );
    assert_eq!(
        env.intent(&bank_kept).await.status,
        intent::SignupStatus::AwaitingBankVerification
    );
    assert_eq!(
        env.intent(&bank_old).await.status,
        intent::SignupStatus::AwaitingBankVerification
    );
    assert_eq!(
        env.intent(&done).await.status,
        intent::SignupStatus::Completed
    );

    let started: Option<NaiveDateTime> = sqlx::query_scalar(
        "SELECT last_started_at FROM job_runs WHERE job_key = 'subscription-maintenance:seats'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let second = env.job().await;
    assert_eq!(second["seat_companies"], 0);
    assert_eq!(second["expired"], 0);
    let again: Option<NaiveDateTime> = sqlx::query_scalar(
        "SELECT last_started_at FROM job_runs WHERE job_key = 'subscription-maintenance:seats'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(started, again);

    let legacy_requests = env
        .stripe
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|seen| seen.path.contains("legacy") || seen.body.contains("legacy"))
        .count();
    assert_eq!(legacy_requests, 0);
    let (legacy_status, plan_source): (String, String) = sqlx::query_as(
        "SELECT cb.subscription_status, CAST(c.plan_source AS CHAR) FROM company_billing cb
           JOIN company c ON c.id = cb.company_id WHERE cb.company_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (legacy_status.as_str(), plan_source.as_str()),
        ("active", "legacy")
    );
}

#[test_db::test(seed)]
async fn the_seat_reconcile_goes_on_where_an_out_of_time_run_stopped(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let done = env
        .ready_signup("owner@pass.test", "Pass Stone", true)
        .await;
    env.complete(&done).await.unwrap();
    // Three subscription companies: this one and the seeded 20400 and 20500.

    // A run already past its 20 s budget reaches no company, and must not
    // count the pass as finished.
    let late = Instant::now()
        .checked_sub(std::time::Duration::from_secs(21))
        .expect("the clock has run for 21 s");
    let stalled = maintenance::run_within(&env.state, late, 25).await.unwrap();
    assert_eq!(stalled.seat_companies + stalled.seat_errors, 0);
    let finished: Option<NaiveDateTime> = sqlx::query_scalar(
        "SELECT last_finished_at FROM job_runs WHERE job_key = 'subscription-maintenance:seats'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(finished, None);

    // Batches of one: each run takes the next company, a fourth finds the end.
    let mut covered = 0;
    for _ in 0..3 {
        let run = maintenance::run_within(&env.state, Instant::now(), 1)
            .await
            .unwrap();
        assert_eq!(run.seat_companies + run.seat_errors, 1);
        covered += run.seat_companies + run.seat_errors;
    }
    assert_eq!(covered, 3);
    let last = maintenance::run_within(&env.state, Instant::now(), 1)
        .await
        .unwrap();
    assert_eq!(last.seat_companies + last.seat_errors, 0);
    // The pass is finished: the next waits its 23 hours.
    let finished: Option<NaiveDateTime> = sqlx::query_scalar(
        "SELECT last_finished_at FROM job_runs WHERE job_key = 'subscription-maintenance:seats'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(finished.is_some());
    let after = maintenance::run_within(&env.state, Instant::now(), 1)
        .await
        .unwrap();
    assert_eq!(after.seat_companies + after.seat_errors, 0);
}

#[test_db::test(seed)]
async fn a_bank_account_not_verified_in_its_window_expires_the_signup_not_the_company(
    pool: MySqlPool,
) {
    let env = Env::new(&pool).await;
    let mut waiting = Vec::new();
    for (email, until, company) in [
        ("owner@late.test", Duration::hours(-1), 100),
        ("owner@inside.test", Duration::days(1), 20500),
    ] {
        let id = env
            .intent_row(
                email,
                "Bank Window",
                "awaiting_bank_verification",
                Duration::days(11),
            )
            .await;
        let customer = env.intent(&id).await.stripe_customer_id.unwrap();
        let setup = env
            .state
            .stripe
            .create_signup_setup_intent(&customer, &id, 1)
            .await
            .unwrap();
        let setup_id = setup["id"].as_str().unwrap().to_string();
        sqlx::query(
            "UPDATE signup_intents
                SET company_id = ?, setup_intent_id = ?, bank_verification_until = ?
              WHERE id = ?",
        )
        .bind(company)
        .bind(&setup_id)
        .bind(Env::now() + until)
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
        // The owner asked to cancel while the bank account verified.
        sqlx::query(
            "INSERT INTO billing_cancellation_requests
               (company_id, requested_by, effective_at, status, requested_at, signup_intent_id)
             VALUES (?, 1, ?, 'scheduled', ?, ?)",
        )
        .bind(company)
        .bind(Env::now() + until)
        .bind(Env::now() - Duration::days(1))
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
        waiting.push((id, customer, setup_id));
    }
    let (late, late_customer, late_setup) = &waiting[0];
    let (inside, _, inside_setup) = &waiting[1];

    let run = env.job().await;
    assert_eq!(run["expired"], 1);

    let gone = env.intent(late).await;
    assert_eq!(gone.status, intent::SignupStatus::Expired);
    assert_eq!(gone.password_hash, "");
    assert_eq!(gone.code_hash, None);
    let cancels = env
        .stripe
        .lock()
        .unwrap()
        .calls("POST", &format!("/v1/setup_intents/{late_setup}/cancel"));
    assert_eq!(cancels.len(), 1);
    assert_eq!(
        cancels[0].idempotency_key.as_deref(),
        Some(format!("signup:{late}:si-cancel:expired").as_str())
    );
    assert_eq!(
        env.stripe.lock().unwrap().setup_intents[late_setup]["status"],
        "canceled"
    );
    // The cancellation asked during the window completes with it; the other
    // still waits.
    let request_status = |signup: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT CAST(status AS CHAR) FROM billing_cancellation_requests
                  WHERE signup_intent_id = ?",
            )
            .bind(signup)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(request_status(late.clone()).await, "completed");
    assert_eq!(request_status(inside.clone()).await, "scheduled");
    // Still inside its window: untouched.
    assert_eq!(
        env.intent(inside).await.status,
        intent::SignupStatus::AwaitingBankVerification
    );
    assert!(
        env.stripe
            .lock()
            .unwrap()
            .calls("POST", &format!("/v1/setup_intents/{inside_setup}/cancel"))
            .is_empty()
    );
    // The company stays, now without a waiting signup (so the first-payment
    // rules of the gate apply), and its Stripe customer is kept: the signup
    // has a company, so the customer cleanup never touches it.
    let companies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM company WHERE id = 100")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(companies, 1);
    assert_eq!(
        api::billing::store::bank_verification_until(&pool, 100)
            .await
            .unwrap(),
        None
    );
    assert!(
        api::billing::store::bank_verification_until(&pool, 20500)
            .await
            .unwrap()
            .is_some(),
        "the signup inside its window still waits"
    );
    let second = env.job().await;
    assert_eq!(second["customers_deleted"], 0);
    assert!(
        env.stripe
            .lock()
            .unwrap()
            .customers
            .contains_key(late_customer)
    );
    assert_eq!(
        env.intent(late).await.stripe_customer_id.as_deref(),
        Some(late_customer.as_str())
    );
}

#[test_db::test(seed)]
async fn customers_that_keep_failing_do_not_block_the_cleanup_batch(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    // 50 expired signups (a full batch) whose customer deletions fail, then
    // a younger one that would otherwise wait behind them forever.
    let mut ids = Vec::new();
    for n in 0..51 {
        let id = env
            .intent_row(
                &format!("owner@stuck{n}.test"),
                "Stuck Stone",
                "expired",
                Duration::days(3) - Duration::minutes(n),
            )
            .await;
        ids.push(id);
    }
    let youngest = ids.pop().unwrap();
    let youngest_customer = env.intent(&youngest).await.stripe_customer_id.unwrap();
    // The first run's 50 deletions fail; the later ones work.
    env.stripe
        .lock()
        .unwrap()
        .fail(Method::DELETE, "/v1/customers/", FailMode::Before, 50);

    let first = env.job().await;
    assert_eq!(first["customers_deleted"], 0);
    assert_eq!(first["errors"], 50);
    assert!(
        env.stripe
            .lock()
            .unwrap()
            .customers
            .contains_key(&youngest_customer)
    );

    // The failed ones went to the back: the next run reaches the youngest.
    let second = env.job().await;
    assert_eq!(second["customers_deleted"], 50);
    assert!(
        !env.stripe
            .lock()
            .unwrap()
            .customers
            .contains_key(&youngest_customer)
    );
    assert_eq!(env.intent(&youngest).await.stripe_customer_id, None);
}

#[test_db::test(seed)]
async fn a_signup_stuck_past_26_hours_is_given_up_with_one_staff_alert(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let stuck = env
        .intent_row(
            "owner@stuck.test",
            "Stuck Stone",
            "verified",
            Duration::hours(30),
        )
        .await;
    let customer = env.intent(&stuck).await.stripe_customer_id.unwrap();
    let backing_off = env
        .intent_row(
            "owner@wait.test",
            "Wait Stone",
            "verified",
            Duration::hours(3),
        )
        .await;
    let now = Env::now();
    sqlx::query(
        "UPDATE signup_intents
            SET status = 'provisioning', accepted_at = ?, updated_at = ?, provision_attempts = 12,
                last_error = 'Stripe error api_error', expires_at = ?
          WHERE id = ?",
    )
    .bind(now - Duration::hours(27))
    .bind(now - Duration::hours(3))
    .bind(now + Duration::hours(1))
    .bind(&stuck)
    .execute(&pool)
    .await
    .unwrap();
    // Three failed runs: the next one waits 80 minutes after the last.
    sqlx::query(
        "UPDATE signup_intents
            SET status = 'provisioning', accepted_at = ?, updated_at = ?, provision_attempts = 3
          WHERE id = ?",
    )
    .bind(now - Duration::hours(2))
    .bind(now - Duration::minutes(30))
    .bind(&backing_off)
    .execute(&pool)
    .await
    .unwrap();

    let run = env.job().await;
    assert_eq!(run["given_up"], 1);
    assert_eq!(run["resumed"], 0);
    assert_eq!(run["customers_deleted"], 1);
    let gone = env.intent(&stuck).await;
    assert_eq!(gone.status, intent::SignupStatus::Expired);
    assert_eq!(gone.password_hash, "");
    assert!(!env.stripe.lock().unwrap().customers.contains_key(&customer));
    assert_eq!(gone.stripe_customer_id, None);
    assert_eq!(
        env.intent(&backing_off).await.status,
        intent::SignupStatus::Provisioning
    );

    let hashed = api::signup::funnel::distinct_id(&stuck);
    {
        let sink = env.sink.lock().unwrap();
        assert_eq!(sink.telegrams.len(), 1);
        let text = sink.telegrams[0]["message"].as_str().unwrap();
        assert!(text.contains(&hashed), "{text}");
        assert!(!text.contains(&stuck));
        assert!(text.contains("Stripe error api_error"));
        let emails = sink.emails_to(STAFF_EMAIL);
        assert_eq!(emails.len(), 1);
        assert!(emails[0]["body"].as_str().unwrap().contains(&hashed));
    }

    // Given up once: the next run alerts no one.
    let again = env.job().await;
    assert_eq!(again["given_up"], 0);
    assert_eq!(env.sink.lock().unwrap().telegrams.len(), 1);
}

#[test_db::test(seed)]
async fn a_refused_signup_whose_customer_survived_is_cleaned_up_by_the_job(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let id = env
        .ready_signup("owner@kept.test", "Kept Stone", true)
        .await;
    let customer = env.intent(&id).await.stripe_customer_id.unwrap();
    sqlx::query(
        "INSERT INTO users (email, password, name, is_employee, company_id)
         VALUES ('owner@kept.test', '', 'Someone', 1, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    env.stripe
        .lock()
        .unwrap()
        .fail(Method::DELETE, "/v1/customers/", FailMode::Before, 1);

    let err = env.complete(&id).await.unwrap_err();
    assert_eq!(code(&err), (409, "email_taken"));
    let refused = env.intent(&id).await;
    assert_eq!(refused.status, intent::SignupStatus::Expired);
    assert_eq!(
        refused.stripe_customer_id.as_deref(),
        Some(customer.as_str())
    );
    assert!(env.stripe.lock().unwrap().customers.contains_key(&customer));

    let run = env.job().await;
    assert_eq!(run["customers_deleted"], 1);
    assert!(!env.stripe.lock().unwrap().customers.contains_key(&customer));
    assert_eq!(env.intent(&id).await.stripe_customer_id, None);
}

#[test]
fn a_free_period_that_already_ended_starts_now() {
    use chrono::TimeZone;
    let now = Utc.with_ymd_and_hms(2027, 1, 10, 12, 0, 0).unwrap();
    let frozen = |day| {
        chrono::NaiveDate::from_ymd_opt(2027, 1, day)
            .unwrap()
            .and_hms_opt(14, 0, 0)
            .unwrap()
    };
    assert_eq!(
        provision::trial_end(frozen(23), now),
        frozen(23).and_utc().timestamp()
    );
    assert_eq!(provision::trial_end(frozen(2), now), now.timestamp());
}

#[test]
fn resumes_back_off_from_ten_minutes_to_two_hours() {
    use api::signup::maintenance::resume_wait;
    let minutes: Vec<i64> = (0..7).map(|n| resume_wait(n).num_minutes()).collect();
    assert_eq!(minutes, [10, 20, 40, 80, 120, 120, 120]);
}
