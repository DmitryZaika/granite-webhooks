//! Self-serve signup paying by bank account (D1): a bank account still
//! waiting for micro-deposits gives the owner the CRM at once, and the
//! subscription follows Stripe's verification (`resume_verified`), once,
//! without a second welcome.

use crate::fake_stripe;
use crate::signup_provisioning::{APP_URL, Env, JOB_KEY, STAFF_EMAIL, code, meta};

use api::app;
use api::signup::consent::PaymentMethodType;
use api::signup::intent::{self, BANK_VERIFICATION_DAYS};
use api::signup::provision::{self, ResumeOutcome};
use axum_test::TestServer;
use chrono::{Duration, NaiveDateTime};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const BANK_METHOD: &str = "pm_bank_20700";
const VERIFY_URL: &str = "https://payments.stripe.com/microdeposit/fake";

/// A `payment_ready` signup whose SetupIntent holds a company bank account
/// waiting for micro-deposits, with its mandate.
async fn pending_bank_signup(env: &Env, email: &str, company: &str) -> (String, String) {
    let id = env.ready_signup(email, company, false).await;
    let setup = env.intent(&id).await.setup_intent_id.unwrap();
    let mut stripe = env.stripe.lock().unwrap();
    let mut method = fake_stripe::bank_account(BANK_METHOD, "STRIPE TEST BANK", "6789");
    method["us_bank_account"]["account_holder_type"] = json!("company");
    stripe.payment_methods.insert(BANK_METHOD.into(), method);
    stripe.pend_microdeposits(&setup, BANK_METHOD);
    stripe.setup_intents.get_mut(&setup).unwrap()["mandate"] = json!("mandate_20700");
    (id, setup)
}

/// A signup completed with that bank account: waiting on it.
async fn awaiting_signup(env: &Env, email: &str, company: &str) -> (String, String) {
    let (id, setup) = pending_bank_signup(env, email, company).await;
    let accepted = env
        .acceptances_for(&id, PaymentMethodType::UsBankAccount)
        .await;
    provision::complete(&env.state, &id, &accepted, &meta())
        .await
        .unwrap();
    (id, setup)
}

/// The SetupIntent as the webhook (or the account page) reads it.
async fn read_setup(env: &Env, id: &str) -> Value {
    env.state.stripe.setup_intent(id).await.unwrap()
}

#[test_db::test(seed)]
async fn a_pending_bank_account_signs_the_owner_in_without_a_subscription(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let email = "owner@bank-pending.test";
    let (id, setup) = pending_bank_signup(&env, email, "Bank Pending Stone").await;
    let accepted = env
        .acceptances_for(&id, PaymentMethodType::UsBankAccount)
        .await;

    let outcome = provision::complete(&env.state, &id, &accepted, &meta())
        .await
        .unwrap();
    let user_id = outcome.sign_in.expect("the owner is signed in at once");
    let intent = env.intent(&id).await;
    assert_eq!(
        intent.status,
        intent::SignupStatus::AwaitingBankVerification
    );
    assert_eq!(intent.password_hash, "");
    assert_eq!(intent.user_id, Some(user_id));
    assert_eq!(intent.stripe_subscription_id, None);
    let accepted_at = intent.accepted_at.unwrap();
    let until = intent.bank_verification_until.unwrap();
    assert_eq!(until, accepted_at + Duration::days(BANK_VERIFICATION_DAYS));
    let bank = outcome.bank_verification.expect("the verify panel");
    assert_eq!(bank.verify_url.as_deref(), Some(VERIFY_URL));
    assert_eq!(bank.verify_by, until.and_utc());
    assert_eq!(bank.bank_name.as_deref(), Some("STRIPE TEST BANK"));
    assert_eq!(bank.last4.as_deref(), Some("6789"));

    // The company and its billing row exist; no subscription, no default method yet.
    let company_id = intent.company_id.unwrap();
    let (status, method, subscription): (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT subscription_status, stripe_payment_method_id, stripe_subscription_id
           FROM company_billing WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), method, subscription),
        ("incomplete", None, None)
    );
    assert!(env.subscription_posts().is_empty());
    let customer = intent.stripe_customer_id.clone().unwrap();
    let links = env
        .stripe
        .lock()
        .unwrap()
        .calls("POST", &format!("/v1/customers/{customer}"));
    assert_eq!(links.len(), 1);
    assert_eq!(
        links[0].idempotency_key.as_deref(),
        Some(format!("signup:{id}:customer-company:awaiting").as_str())
    );
    assert_eq!(
        links[0].field("invoice_settings[default_payment_method]"),
        None
    );
    assert_eq!(
        links[0].field("metadata[company_id]"),
        Some(company_id.to_string())
    );

    // The base checkboxes and the ACH one, with the mandate and SEC code.
    type ConsentRow = (String, Option<String>, Option<String>, Option<String>);
    let rows: Vec<ConsentRow> = sqlx::query_as(
        "SELECT kind, payment_method_id, stripe_mandate_id, CAST(sec_code_intended AS CHAR)
           FROM consent_records WHERE signup_intent_id = ? ORDER BY id",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let kinds: Vec<&str> = rows.iter().map(|row| row.0.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "terms",
            "terms",
            "terms",
            "auto_renewal",
            "per_user_pricing",
            "onboarding_fee",
            "ach_authorization"
        ]
    );
    let ach = rows.last().unwrap();
    assert_eq!(
        (ach.1.as_deref(), ach.2.as_deref(), ach.3.as_deref()),
        (Some(BANK_METHOD), Some("mandate_20700"), Some("CCD"))
    );
    assert!(
        rows[..6]
            .iter()
            .all(|row| row.2.is_none() && row.3.is_none())
    );
    let amounts: Value = serde_json::from_str(
        &sqlx::query_scalar::<_, String>(
            "SELECT CAST(amounts_and_dates AS CHAR) FROM consent_records
              WHERE signup_intent_id = ? AND kind = 'ach_authorization'",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(amounts["payment_method_type"], "us_bank_account");
    assert_eq!(amounts["payment_method_last4"], "6789");
    assert_eq!(amounts["payment_method_bank_name"], "STRIPE TEST BANK");

    // Welcomed once; a second completion answers the same without a session.
    assert_eq!(env.notices(email), (1, 1, 1));
    let again = env.complete(&id).await.unwrap();
    assert_eq!(again.sign_in, None);
    assert_eq!(
        again.bank_verification.map(|bank| bank.verify_by),
        Some(until.and_utc())
    );
    assert_eq!(again.redirect_url, format!("{APP_URL}/employee?welcome=1"));
    assert_eq!(env.notices(email), (1, 1, 1));

    // The page reads the saved method live.
    let server = TestServer::new(app(env.state.clone()));
    let signup: Value = server.get(&format!("/v1/public/signups/{id}")).await.json();
    assert_eq!(signup["status"], "awaiting_bank_verification");
    assert_eq!(signup["payment_method"]["status"], "verifying");
    assert_eq!(signup["payment_method"]["type"], "us_bank_account");
    assert_eq!(signup["payment_method"]["brand"], "STRIPE TEST BANK");
    assert_eq!(signup["payment_method"]["verify_url"], VERIFY_URL);
    let verify_by: chrono::DateTime<chrono::Utc> = signup["payment_method"]["verify_by"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(verify_by, until.and_utc());
    assert!(
        env.stripe
            .lock()
            .unwrap()
            .setup_intents
            .contains_key(&setup)
    );
}

#[test_db::test(seed)]
async fn a_signup_waiting_on_its_bank_is_never_expired_or_resumed_by_the_job(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let (id, _) = awaiting_signup(&env, "owner@bank-old.test", "Bank Old Stone").await;
    let long_ago: NaiveDateTime = Env::now() - Duration::days(40);
    sqlx::query(
        "UPDATE signup_intents
            SET updated_at = ?, expires_at = ?, accepted_at = ?, bank_verification_until = ?
          WHERE id = ?",
    )
    .bind(long_ago)
    .bind(long_ago)
    .bind(long_ago)
    .bind(long_ago + Duration::days(BANK_VERIFICATION_DAYS))
    .bind(&id)
    .execute(&pool)
    .await
    .unwrap();
    let server = TestServer::new(app(env.state.clone()));
    let run: Value = server
        .post("/v1/jobs/subscription-maintenance")
        .add_header("authorization", JOB_KEY)
        .await
        .json();
    assert_eq!(run["expired"], 0);
    assert_eq!(run["resumed"], 0);
    assert_eq!(run["given_up"], 0);
    let intent = env.intent(&id).await;
    assert_eq!(
        intent.status,
        intent::SignupStatus::AwaitingBankVerification
    );
    assert!(!intent.is_expired(Env::now()));
    assert!(intent.stripe_customer_id.is_some());
    server
        .get(&format!("/v1/public/signups/{id}"))
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn a_verified_bank_account_makes_the_subscription_once_without_a_second_welcome(
    pool: MySqlPool,
) {
    let env = Env::new(&pool).await;
    let email = "owner@bank-verified.test";
    let (id, setup) = awaiting_signup(&env, email, "Bank Verified Stone").await;
    env.stripe
        .lock()
        .unwrap()
        .confirm_setup_intent(&setup, BANK_METHOD);
    let verified = read_setup(&env, &setup).await;

    let outcome = provision::resume_verified(&env.state, &id, &verified)
        .await
        .unwrap();
    assert_eq!(outcome, ResumeOutcome::Completed);
    let intent = env.intent(&id).await;
    assert_eq!(intent.status, intent::SignupStatus::Completed);
    assert!(intent.bank_verification_until.is_some());
    let posts = env.subscription_posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].field("default_payment_method").as_deref(),
        Some(BANK_METHOD)
    );
    assert_eq!(
        posts[0].field("trial_end"),
        Some(
            intent
                .trial_end_at
                .unwrap()
                .and_utc()
                .timestamp()
                .to_string()
        )
    );
    let customer = intent.stripe_customer_id.clone().unwrap();
    let link = env
        .stripe
        .lock()
        .unwrap()
        .calls("POST", &format!("/v1/customers/{customer}"))
        .into_iter()
        .find(|seen| {
            seen.idempotency_key.as_deref() == Some(&format!("signup:{id}:customer-company"))
        })
        .expect("the customer gets its default method");
    assert_eq!(
        link.field("invoice_settings[default_payment_method]")
            .as_deref(),
        Some(BANK_METHOD)
    );
    let (status, subscription): (String, Option<String>) = sqlx::query_as(
        "SELECT subscription_status, stripe_subscription_id FROM company_billing WHERE company_id = ?",
    )
    .bind(intent.company_id.unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "trialing");
    assert_eq!(subscription, intent.stripe_subscription_id);
    // No second welcome or staff notice.
    assert_eq!(env.notices(email), (1, 1, 1));

    // A duplicate webhook: nothing more.
    let again = provision::resume_verified(&env.state, &id, &verified)
        .await
        .unwrap();
    assert_eq!(again, ResumeOutcome::NotAwaiting);
    assert_eq!(env.subscriptions(), 1);
    assert_eq!(env.subscription_posts().len(), 1);
    assert_eq!(env.notices(email), (1, 1, 1));
    // Completion now answers without the verify panel.
    let done = env.complete(&id).await.unwrap();
    assert_eq!((done.sign_in, done.bank_verification), (None, None));
}

#[test_db::test(seed)]
async fn two_resumes_at_once_make_one_subscription(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let email = "owner@bank-race.test";
    let (id, setup) = awaiting_signup(&env, email, "Bank Race Stone").await;
    env.stripe
        .lock()
        .unwrap()
        .confirm_setup_intent(&setup, BANK_METHOD);
    let verified = read_setup(&env, &setup).await;
    let (first, second) = tokio::join!(
        provision::resume_verified(&env.state, &id, &verified),
        provision::resume_verified(&env.state, &id, &verified),
    );
    let outcomes = [first.unwrap(), second.unwrap()];
    assert!(outcomes.contains(&ResumeOutcome::Completed));
    assert_eq!(
        env.intent(&id).await.status,
        intent::SignupStatus::Completed
    );
    assert_eq!(env.subscriptions(), 1);
    assert_eq!(env.notices(email), (1, 1, 1));
}

#[test_db::test(seed)]
async fn a_card_added_while_waiting_replaces_the_bank_account(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let (id, old) = awaiting_signup(&env, "owner@bank-card.test", "Bank Card Stone").await;
    let intent = env.intent(&id).await;
    let customer = intent.stripe_customer_id.clone().unwrap();
    let company_id = intent.company_id.unwrap();
    let created = env
        .state
        .stripe
        .create_company_setup_intent(
            &customer,
            company_id,
            "payment_method_update",
            None,
            "billing-setup:20700:test",
        )
        .await
        .unwrap();
    let new = created["id"].as_str().unwrap().to_string();
    env.stripe
        .lock()
        .unwrap()
        .confirm_setup_intent(&new, "pm_card_visa");
    let card = read_setup(&env, &new).await;

    let outcome = provision::resume_verified(&env.state, &id, &card)
        .await
        .unwrap();
    assert_eq!(outcome, ResumeOutcome::Completed);
    let done = env.intent(&id).await;
    assert_eq!(done.status, intent::SignupStatus::Completed);
    assert_eq!(done.setup_intent_id.as_deref(), Some(new.as_str()));
    assert_eq!(done.payment_method_id.as_deref(), Some("pm_card_visa"));
    let cancels = env
        .stripe
        .lock()
        .unwrap()
        .calls("POST", &format!("/v1/setup_intents/{old}/cancel"));
    assert_eq!(cancels.len(), 1);
    assert_eq!(
        cancels[0].idempotency_key.as_deref(),
        Some(format!("signup:{id}:si-cancel:{old}").as_str())
    );
    let posts = env.subscription_posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].field("default_payment_method").as_deref(),
        Some("pm_card_visa")
    );
}

#[test_db::test(seed)]
async fn a_setup_intent_of_another_customer_is_refused(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let (id, _) = awaiting_signup(&env, "owner@bank-other.test", "Bank Other Stone").await;
    let stranger = env
        .customer(&uuid::Uuid::new_v4().to_string(), "x@other.test")
        .await;
    let created = env
        .state
        .stripe
        .create_company_setup_intent(&stranger, 1, "payment_method_update", None, "other:20701")
        .await
        .unwrap();
    let other = created["id"].as_str().unwrap().to_string();
    env.stripe
        .lock()
        .unwrap()
        .confirm_setup_intent(&other, "pm_card_visa");
    let setup = read_setup(&env, &other).await;
    let err = provision::resume_verified(&env.state, &id, &setup)
        .await
        .unwrap_err();
    assert_eq!(code(&err), (409, "setup_intent_mismatch"));
    assert_eq!(
        env.intent(&id).await.status,
        intent::SignupStatus::AwaitingBankVerification
    );
    assert!(env.subscription_posts().is_empty());
}

#[test_db::test(seed)]
async fn a_failed_setup_is_not_ready_and_the_wrong_payment_checkbox_is_refused(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    // The bank account could not be linked.
    let failed = env
        .ready_signup("owner@bank-failed.test", "Bank Failed Stone", false)
        .await;
    let setup = env.intent(&failed).await.setup_intent_id.unwrap();
    env.stripe.lock().unwrap().fail_setup_intent(
        &setup,
        "payment_method_microdeposit_verification_attempts_exceeded",
    );
    let accepted = env
        .acceptances_for(&failed, PaymentMethodType::UsBankAccount)
        .await;
    let err = provision::complete(&env.state, &failed, &accepted, &meta())
        .await
        .unwrap_err();
    assert_eq!(code(&err), (409, "payment_not_ready"));
    assert_eq!(
        env.intent(&failed).await.status,
        intent::SignupStatus::PaymentReady
    );

    // A bank account with the card checkbox, and a card with the ACH one.
    let (bank, _) = pending_bank_signup(&env, "owner@bank-box.test", "Bank Box Stone").await;
    let card_box = env.acceptances_for(&bank, PaymentMethodType::Card).await;
    let err = provision::complete(&env.state, &bank, &card_box, &meta())
        .await
        .unwrap_err();
    assert_eq!(code(&err), (409, "consents_changed"));
    let card = env
        .ready_signup("owner@card-box.test", "Card Box Stone", true)
        .await;
    let ach_box = env
        .acceptances_for(&card, PaymentMethodType::UsBankAccount)
        .await;
    let err = provision::complete(&env.state, &card, &ach_box, &meta())
        .await
        .unwrap_err();
    assert_eq!(code(&err), (409, "consents_changed"));
    for id in [&bank, &card] {
        let records = env
            .count(
                "SELECT COUNT(*) FROM consent_records WHERE signup_intent_id = ?",
                id,
            )
            .await;
        assert_eq!(records, 0);
    }
    assert_eq!(env.sink.lock().unwrap().emails_to(STAFF_EMAIL).len(), 0);
}

#[test_db::test(seed)]
async fn a_bank_verified_after_the_first_charge_day_is_billed_now(pool: MySqlPool) {
    let env = Env::new(&pool).await;
    let (id, setup) = awaiting_signup(&env, "owner@bank-late.test", "Bank Late Stone").await;
    sqlx::query("UPDATE signup_intents SET trial_end_at = ? WHERE id = ?")
        .bind(Env::now() - Duration::days(1))
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    env.stripe
        .lock()
        .unwrap()
        .confirm_setup_intent(&setup, BANK_METHOD);
    let verified = read_setup(&env, &setup).await;
    let outcome = provision::resume_verified(&env.state, &id, &verified)
        .await
        .unwrap();
    assert_eq!(outcome, ResumeOutcome::Completed);
    let posts = env.subscription_posts();
    assert_eq!(posts.len(), 1);
    // Stripe takes `now` for a free period that already ended.
    assert_eq!(posts[0].field("trial_end").as_deref(), Some("now"));
}
