use crate::common;
use crate::fake_stripe;
use crate::fake_webhooks;

use api::app;
use api::signup::SignupConfig;
use api::state::AppState;
use axum::http::StatusCode;
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

struct Env {
    server: TestServer,
    stripe: fake_stripe::Shared,
    sink: fake_webhooks::Shared,
    pool: MySqlPool,
}

async fn env_with(pool: MySqlPool, config: SignupConfig) -> Env {
    let stripe = fake_stripe::FakeStripe::with_catalog();
    let sink = Arc::new(Mutex::new(fake_webhooks::Sink::default()));
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_stripe(fake_stripe::client(&stripe).await)
        .with_webhooks(fake_webhooks::start(&sink).await)
        .with_app_url("https://app.example")
        .with_signup(config);
    Env {
        server: TestServer::new(app(state)),
        stripe,
        sink,
        pool,
    }
}

async fn env(pool: MySqlPool) -> Env {
    env_with(
        pool,
        SignupConfig {
            enabled: true,
            test_payment_method: Some("pm_card_visa".into()),
            ..Default::default()
        },
    )
    .await
}

fn body(email: &str) -> Value {
    json!({
        "plan": { "users": 12, "addons": ["quotes"] },
        "company": {
            "name": "Fresh Stone", "phone": "+13175550100", "website": null,
            "street": "1 Main St", "city": "Indianapolis", "state": "IN", "zip": "46204"
        },
        "owner": { "name": "Fran Fresh", "email": email, "password": "correct horse" }
    })
}

/// The 6-digit code in the last email to `to`: the element holding just six
/// digits (colors like `#475569` are not it).
fn code_for(env: &Env, to: &str) -> String {
    let sink = env.sink.lock().unwrap();
    let email = sink.emails_to(to).pop().expect("a code email");
    let html = email["body"].as_str().unwrap();
    html.split('>')
        .filter_map(|part| part.split('<').next())
        .map(str::trim)
        .find(|text| text.len() == 6 && text.bytes().all(|b| b.is_ascii_digit()))
        .expect("a code in the email")
        .to_string()
}

async fn start(env: &Env, email: &str) -> String {
    let res = env
        .server
        .post("/v1/public/signups")
        .json(&body(email))
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json::<Value>()["id"].as_str().unwrap().to_string()
}

async fn verify(env: &Env, id: &str, code: &str) -> axum_test::TestResponse {
    env.server
        .post(&format!("/v1/public/signups/{id}/verification"))
        .json(&json!({ "code": code }))
        .await
}

async fn verified(env: &Env, email: &str) -> String {
    let id = start(env, email).await;
    let code = code_for(env, email);
    verify(env, &id, &code).await.assert_status_ok();
    id
}

fn wrong(code: &str) -> &'static str {
    if code == "000000" { "000001" } else { "000000" }
}

fn sha(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test_db::test(seed)]
async fn plans_quote_is_exact(pool: MySqlPool) {
    let env = env(pool).await;
    let res = env
        .server
        .get("/v1/public/plans?users=12&addon=quotes&addon=bogus")
        .await;
    res.assert_status_ok();
    let mut plans: Value = res.json();
    let quote = plans["quote"].as_object_mut().unwrap();
    assert!(quote.remove("first_charge_on").unwrap().is_string());
    assert!(quote.remove("deadline_on").unwrap().is_string());
    assert_eq!(
        plans,
        json!({
            "base": { "name": "Base plan", "monthly_cents": 30000, "included_users": 10 },
            "extra_user_cents": 3000,
            "free_months": 3,
            "min_users": 1,
            "max_users": 500,
            "addons": [
                { "key": "signatures", "name": "Signatures",
                  "description": "eSignatures and an email signature block", "monthly_cents": 1500 },
                { "key": "quotes", "name": "Quotes",
                  "description": "Quote editor, price lists and online quotes", "monthly_cents": 7500 },
                { "key": "quickbooks", "name": "QuickBooks",
                  "description": "Invoices and QuickBooks sync", "monthly_cents": 3000 }
            ],
            "quote": {
                "users": 12, "included_users": 10, "extra_users": 2,
                "lines": [
                    { "kind": "base", "key": null, "label": "Base plan (up to 10 users)",
                      "quantity": 1, "unit_cents": 30000, "amount_cents": 30000 },
                    { "kind": "extra_users", "key": null, "label": "2 extra users",
                      "quantity": 2, "unit_cents": 3000, "amount_cents": 6000 },
                    { "kind": "addon", "key": "quotes", "label": "Quotes",
                      "quantity": 1, "unit_cents": 7500, "amount_cents": 7500 }
                ],
                "base_and_addons_cents": 37500, "monthly_cents": 43500,
                "due_today_cents": 0, "advance_cents": 43500
            },
            "signups_enabled": true
        })
    );
}

#[test_db::test(seed)]
async fn plans_defaults_and_clamps(pool: MySqlPool) {
    let env = env(pool).await;
    let plans: Value = env.server.get("/v1/public/plans").await.json();
    assert_eq!(plans["quote"]["users"], 5);
    assert_eq!(plans["quote"]["monthly_cents"], 30000);
    let plans: Value = env.server.get("/v1/public/plans?users=9999").await.json();
    assert_eq!(plans["quote"]["users"], 500);
    let plans: Value = env.server.get("/v1/public/plans?users=0").await.json();
    assert_eq!(plans["quote"]["users"], 1);
}

#[test_db::test(seed)]
async fn the_kill_switch_closes_signups_but_not_plans(pool: MySqlPool) {
    let env = env_with(pool, SignupConfig::default()).await;
    let plans = env.server.get("/v1/public/plans").await;
    plans.assert_status_ok();
    let plans: Value = plans.json();
    assert_eq!(plans["signups_enabled"], false);
    assert_eq!(plans["addons"].as_array().unwrap().len(), 3);
    let id = "11111111-1111-4111-8111-111111111111";
    let calls = [
        env.server
            .post("/v1/public/signups")
            .json(&body("a@b.test"))
            .await,
        env.server.get(&format!("/v1/public/signups/{id}")).await,
        env.server
            .put(&format!("/v1/public/signups/{id}/plan"))
            .json(&json!({ "users": 3, "addons": [] }))
            .await,
        env.server
            .post(&format!("/v1/public/signups/{id}/code"))
            .await,
        verify(&env, id, "123456").await,
        env.server
            .get(&format!("/v1/public/signups/{id}/consents"))
            .await,
        env.server
            .post(&format!("/v1/public/signups/{id}/payment-setup"))
            .await,
        env.server
            .post(&format!("/v1/public/signups/{id}/completion"))
            .json(&json!({ "consents": [] }))
            .await,
    ];
    for res in calls {
        res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.json::<Value>()["code"], "signup_unavailable");
    }
}

#[test_db::test(seed)]
async fn create_stores_the_signup_and_emails_a_code(pool: MySqlPool) {
    let env = env(pool).await;
    let res = env
        .server
        .post("/v1/public/signups")
        .add_header("user-agent", "test-agent")
        .json(&body("Fran@Fresh.test"))
        .await;
    res.assert_status(StatusCode::CREATED);
    let created: Value = res.json();
    assert_eq!(created["resend_in_seconds"], 60);
    let id = created["id"].as_str().unwrap();

    let email = env
        .sink
        .lock()
        .unwrap()
        .emails_to("fran@fresh.test")
        .pop()
        .unwrap();
    assert_eq!(email["subject"], "Your Granite Manager code");
    assert_eq!(email["transactional"], true);
    assert!(!email["body"].as_str().unwrap().contains('\u{2014}'));
    let code = code_for(&env, "fran@fresh.test");

    let (status, hash, password, sent, attempts, agent): (
        String,
        String,
        String,
        u8,
        u8,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, code_hash, password_hash, codes_sent, code_attempts, user_agent
               FROM signup_intents WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(status, "started");
    assert_eq!(hash.len(), 64);
    assert_ne!(hash, code);
    assert!(password.starts_with("$2"));
    assert_eq!((sent, attempts), (1, 0));
    assert_eq!(agent.as_deref(), Some("test-agent"));

    let signup: Value = env
        .server
        .get(&format!("/v1/public/signups/{id}"))
        .await
        .json();
    assert_eq!(signup["email_masked"], "f***@fresh.test");
    assert_eq!(signup["status"], "started");
    assert_eq!(signup["company_name"], "Fresh Stone");
    assert_eq!(signup["payment_mode"], "test");
    assert_eq!(signup["publishable_key"], Value::Null);
    assert_eq!(signup["payment_method"], Value::Null);
    assert!(signup["resend_in_seconds"].as_i64().unwrap() > 0);
    assert_eq!(signup["quote"]["monthly_cents"], 43500);
}

/// Whether the last email to `to` holds a 6-digit code.
fn has_code(env: &Env, to: &str) -> bool {
    let sink = env.sink.lock().unwrap();
    let email = sink.emails_to(to).pop().expect("an email");
    email["body"]
        .as_str()
        .unwrap()
        .split('>')
        .filter_map(|part| part.split('<').next())
        .any(|text| {
            let text = text.trim();
            text.len() == 6 && text.bytes().all(|b| b.is_ascii_digit())
        })
}

fn last_email(env: &Env, to: &str) -> Value {
    env.sink
        .lock()
        .unwrap()
        .emails_to(to)
        .pop()
        .expect("an email")
}

async fn backdate_code(env: &Env, id: &str) {
    sqlx::query(
        "UPDATE signup_intents SET code_sent_at = code_sent_at - INTERVAL 2 MINUTE WHERE id = ?",
    )
    .bind(id)
    .execute(&env.pool)
    .await
    .unwrap();
}

/// S3: an email that already has an account gets the same answer as a new
/// one; the address is told so (no code), and the signup never verifies.
#[test_db::test(seed)]
async fn existing_emails_get_the_same_answer_and_no_code(pool: MySqlPool) {
    let env = env(pool).await;
    let start_json = |email: &'static str| {
        let env = &env;
        async move {
            let res = env
                .server
                .post("/v1/public/signups")
                .json(&body(email))
                .await;
            res.assert_status(StatusCode::CREATED);
            res.json::<Value>()
        }
    };
    let fresh = start_json("new@fresh.test").await;
    let taken = start_json("  REP@Acme.Test ").await;
    let gone = start_json("gone@acme.test").await;
    for created in [&fresh, &taken, &gone] {
        let keys: Vec<&String> = created.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["id", "resend_in_seconds"]);
        assert_eq!(created["resend_in_seconds"], 60);
    }
    assert!(has_code(&env, "new@fresh.test"));

    let email = last_email(&env, "rep@acme.test");
    assert_eq!(email["subject"], "Your Granite Manager signup");
    let html = email["body"].as_str().unwrap();
    assert!(html.contains("You already have an account"));
    assert!(html.contains(r#"href="https://app.example/login""#));
    assert!(html.contains("Forgot password?"));
    assert!(!has_code(&env, "rep@acme.test"));
    let email = last_email(&env, "gone@acme.test");
    let html = email["body"].as_str().unwrap();
    assert!(html.contains("This email belonged to a deleted account"));
    assert!(html.contains("a different email"));
    assert!(!has_code(&env, "gone@acme.test"));

    // The signup reads like any other and fails verification like a wrong code.
    let id = taken["id"].as_str().unwrap();
    let signup: Value = env
        .server
        .get(&format!("/v1/public/signups/{id}"))
        .await
        .json();
    assert_eq!(signup["status"], "started");
    assert_eq!(signup["email_masked"], "r***@acme.test");
    for code in ["000000", "123456"] {
        let res = verify(&env, id, code).await;
        res.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(res.json::<Value>()["code"], "invalid_code");
    }
    let hash: String = sqlx::query_scalar("SELECT code_hash FROM signup_intents WHERE id = ?")
        .bind(id)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(hash, "none");

    // A resend repeats the account-exists email, still without a code.
    backdate_code(&env, id).await;
    let resent = env
        .server
        .post(&format!("/v1/public/signups/{id}/code"))
        .await;
    resent.assert_status_ok();
    assert_eq!(resent.json::<Value>()["resend_in_seconds"], 60);
    assert_eq!(env.sink.lock().unwrap().emails_to("rep@acme.test").len(), 2);
    assert!(!has_code(&env, "rep@acme.test"));
    let res = verify(&env, id, "000000").await;
    res.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(res.json::<Value>()["code"], "invalid_code");
}

fn assert_too_many(res: &axum_test::TestResponse) {
    res.assert_status(StatusCode::TOO_MANY_REQUESTS);
    let json: Value = res.json();
    assert_eq!(json["code"], "too_many_signups");
    let wait = json["retry_after_seconds"].as_i64().unwrap();
    assert!((3500..=3600).contains(&wait), "{wait}");
    let header: i64 = res
        .headers()
        .get("retry-after")
        .expect("Retry-After")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(header, wait);
}

/// S3: three signups per email per hour, existing accounts included (no
/// email bombing), and the per-signup resend still works.
#[test_db::test(seed)]
async fn signups_per_email_are_limited_per_hour(pool: MySqlPool) {
    let env = env(pool).await;
    for email in ["many@fresh.test", "rep@acme.test"] {
        let first = start(&env, email).await;
        start(&env, email).await;
        start(&env, &email.to_uppercase()).await;
        let fourth = env
            .server
            .post("/v1/public/signups")
            .json(&body(email))
            .await;
        assert_too_many(&fourth);
        assert_eq!(env.sink.lock().unwrap().emails_to(email).len(), 3);

        backdate_code(&env, &first).await;
        env.server
            .post(&format!("/v1/public/signups/{first}/code"))
            .await
            .assert_status_ok();
        assert_eq!(env.sink.lock().unwrap().emails_to(email).len(), 4);
    }
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM signup_intents")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(rows, 6);

    // Signups older than an hour no longer count.
    sqlx::query(
        "UPDATE signup_intents SET created_at = created_at - INTERVAL 61 MINUTE
          WHERE email = 'many@fresh.test'",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    start(&env, "many@fresh.test").await;
}

/// `SIGNUP_RATE_LIMITS=off` lifts the limits for e2e runs, with a test key
/// only: a live key keeps them.
#[test_db::test(seed)]
async fn the_rate_limits_are_lifted_only_with_a_test_key(pool: MySqlPool) {
    let lifted = SignupConfig {
        enabled: true,
        rate_limits_off: true,
        ..Default::default()
    };
    let env = env_with(pool.clone(), lifted.clone()).await;
    for n in 0..12 {
        env.server
            .post("/v1/public/signups")
            .add_header("x-forwarded-for", "203.0.113.9")
            .json(&body(&format!("lifted{n}@fresh.test")))
            .await
            .assert_status(StatusCode::CREATED);
    }

    let stripe = fake_stripe::FakeStripe::with_catalog();
    let sink = Arc::new(Mutex::new(fake_webhooks::Sink::default()));
    let live = api::billing::stripe::Stripe::new(
        Some("sk_live_fake"),
        &fake_stripe::start(Arc::clone(&stripe)).await,
    );
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN)
        .with_stripe(live)
        .with_webhooks(fake_webhooks::start(&sink).await)
        .with_app_url("https://app.example")
        .with_signup(lifted);
    let server = TestServer::new(app(state));
    let mut last = None;
    for n in 0..11 {
        last = Some(
            server
                .post("/v1/public/signups")
                .add_header("x-forwarded-for", "198.51.100.9")
                .json(&body(&format!("live{n}@fresh.test")))
                .await,
        );
    }
    assert_too_many(&last.unwrap());
}

/// S3: ten signups per client IP per hour, whatever the email.
#[test_db::test(seed)]
async fn signups_per_ip_are_limited_per_hour(pool: MySqlPool) {
    let env = env(pool).await;
    let from = |ip: &'static str, n: usize| {
        let env = &env;
        async move {
            env.server
                .post("/v1/public/signups")
                .add_header("x-forwarded-for", ip)
                .json(&body(&format!("ip{n}@fresh.test")))
                .await
        }
    };
    for n in 0..10 {
        from("203.0.113.7", n)
            .await
            .assert_status(StatusCode::CREATED);
    }
    assert_too_many(&from("203.0.113.7", 10).await);
    // The payments app (another origin) can read the wait.
    let limited = env
        .server
        .post("/v1/public/signups")
        .add_header("x-forwarded-for", "203.0.113.7")
        .add_header("origin", ALLOWED_ORIGIN)
        .json(&body("ip-cors@fresh.test"))
        .await;
    assert_too_many(&limited);
    assert!(
        limited
            .header("access-control-expose-headers")
            .to_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("retry-after")
    );
    from("198.51.100.4", 11)
        .await
        .assert_status(StatusCode::CREATED);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM signup_intents")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(rows, 11);
}

#[test_db::test(seed)]
async fn invalid_details_are_refused_before_any_write(pool: MySqlPool) {
    let env = env(pool).await;
    type Edit = (&'static str, Box<dyn Fn(&mut Value)>, &'static str);
    let edits: [Edit; 8] = [
        (
            "short password",
            Box::new(|b| b["owner"]["password"] = json!("123456789")),
            "Password must be at least 10 characters long.",
        ),
        (
            "padded password",
            Box::new(|b| b["owner"]["password"] = json!("   short   ")),
            "Password must be at least 10 characters long.",
        ),
        (
            "email",
            Box::new(|b| b["owner"]["email"] = json!("nope")),
            "Enter a valid email",
        ),
        (
            "state",
            Box::new(|b| b["company"]["state"] = json!("Indiana")),
            "Enter a two-letter state, like IN.",
        ),
        (
            "zip",
            Box::new(|b| b["company"]["zip"] = json!("4620")),
            "Enter a ZIP code like 46204 or 46204-1234.",
        ),
        (
            "phone",
            Box::new(|b| b["company"]["phone"] = json!("3175550100")),
            "Enter a phone number with the country code, like +13175550100.",
        ),
        (
            "name",
            Box::new(|b| b["company"]["name"] = json!("  ")),
            "Enter the company name.",
        ),
        (
            "owner",
            Box::new(|b| b["owner"]["name"] = json!("x".repeat(81))),
            "The owner name can be up to 80 characters.",
        ),
    ];
    for (what, edit, message) in edits {
        let mut payload = body("valid@fresh.test");
        edit(&mut payload);
        let res = env.server.post("/v1/public/signups").json(&payload).await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], message, "{what}");
    }
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM signup_intents")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[test_db::test(seed)]
async fn a_failed_email_drops_the_signup(pool: MySqlPool) {
    let env = env(pool).await;
    env.sink.lock().unwrap().fail_emails = 1;
    let res = env
        .server
        .post("/v1/public/signups")
        .json(&body("down@fresh.test"))
        .await;
    res.assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(
        res.json::<Value>()["error"],
        "We could not send the email. Try again in a minute."
    );
    let (status, password): (String, String) = sqlx::query_as(
        "SELECT status, password_hash FROM signup_intents WHERE email = 'down@fresh.test'",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(status, "expired");
    assert_eq!(password, "");
}

#[test_db::test(seed)]
async fn five_wrong_codes_lock_until_a_new_code(pool: MySqlPool) {
    let env = env(pool).await;
    let id = start(&env, "lock@fresh.test").await;
    let code = code_for(&env, "lock@fresh.test");
    let bad = wrong(&code);
    for left in [4, 3, 2, 1] {
        let res = verify(&env, &id, bad).await;
        res.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        let json: Value = res.json();
        assert_eq!(json["code"], "invalid_code");
        assert_eq!(json["attempts_left"], left);
    }
    let fifth = verify(&env, &id, bad).await;
    fifth.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(fifth.json::<Value>()["code"], "too_many_attempts");
    let locked = verify(&env, &id, &code).await;
    locked.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(locked.json::<Value>()["code"], "too_many_attempts");

    sqlx::query(
        "UPDATE signup_intents SET code_sent_at = code_sent_at - INTERVAL 2 MINUTE WHERE id = ?",
    )
    .bind(&id)
    .execute(&env.pool)
    .await
    .unwrap();
    env.server
        .post(&format!("/v1/public/signups/{id}/code"))
        .await
        .assert_status_ok();
    let fresh = code_for(&env, "lock@fresh.test");
    let res = verify(&env, &id, &fresh).await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["status"], "verified");
    // Verified: answered without checking the code.
    let again = verify(&env, &id, "999999").await;
    again.assert_status_ok();
    assert_eq!(again.json::<Value>()["status"], "verified");
}

#[test_db::test(seed)]
async fn the_fifth_attempt_can_still_be_right(pool: MySqlPool) {
    let env = env(pool).await;
    let id = start(&env, "late@fresh.test").await;
    let code = code_for(&env, "late@fresh.test");
    for _ in 0..4 {
        verify(&env, &id, wrong(&code)).await;
    }
    verify(&env, &id, &code).await.assert_status_ok();
}

#[test_db::test(seed)]
async fn resend_is_limited_by_time_and_count(pool: MySqlPool) {
    let env = env(pool).await;
    let id = start(&env, "again@fresh.test").await;
    let path = format!("/v1/public/signups/{id}/code");
    let soon = env.server.post(&path).await;
    soon.assert_status(StatusCode::TOO_MANY_REQUESTS);
    let json: Value = soon.json();
    assert_eq!(json["code"], "resend_limited");
    let wait = json["retry_after_seconds"].as_i64().unwrap();
    assert!((1..=60).contains(&wait));

    let backdate = || async {
        sqlx::query("UPDATE signup_intents SET code_sent_at = code_sent_at - INTERVAL 2 MINUTE WHERE id = ?")
            .bind(&id)
            .execute(&env.pool)
            .await
            .unwrap();
    };
    for _ in 0..4 {
        backdate().await;
        let res = env.server.post(&path).await;
        res.assert_status_ok();
        assert_eq!(res.json::<Value>()["resend_in_seconds"], 60);
    }
    assert_eq!(
        env.sink.lock().unwrap().emails_to("again@fresh.test").len(),
        5
    );
    backdate().await;
    let capped = env.server.post(&path).await;
    capped.assert_status(StatusCode::TOO_MANY_REQUESTS);
    let json: Value = capped.json();
    assert_eq!(json["code"], "resend_limited");
    assert_eq!(
        json["error"],
        "You have asked for 5 codes. Start again or email support@granite-manager.com."
    );
    assert!(json.get("retry_after_seconds").is_none());
}

#[test_db::test(seed)]
async fn an_old_code_is_expired(pool: MySqlPool) {
    let env = env(pool).await;
    let id = start(&env, "old@fresh.test").await;
    let code = code_for(&env, "old@fresh.test");
    sqlx::query("UPDATE signup_intents SET code_expires_at = code_expires_at - INTERVAL 20 MINUTE WHERE id = ?")
        .bind(&id)
        .execute(&env.pool)
        .await
        .unwrap();
    let res = verify(&env, &id, &code).await;
    res.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(res.json::<Value>()["code"], "code_expired");
}

#[test_db::test(seed)]
async fn consents_follow_verification_and_the_plan(pool: MySqlPool) {
    let env = env(pool).await;
    let id = start(&env, "terms@fresh.test").await;
    let path = format!("/v1/public/signups/{id}/consents");
    let early = env.server.get(&path).await;
    early.assert_status(StatusCode::CONFLICT);
    assert_eq!(early.json::<Value>()["code"], "invalid_state");

    let code = code_for(&env, "terms@fresh.test");
    verify(&env, &id, &code).await.assert_status_ok();
    let consents: Value = env.server.get(&path).await.json();
    let list = consents["consents"].as_array().unwrap();
    let kinds: Vec<_> = list
        .iter()
        .map(|c| {
            (
                c["kind"].as_str().unwrap(),
                c["payment_method_type"].clone(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            ("terms", Value::Null),
            ("auto_renewal", Value::Null),
            ("per_user_pricing", Value::Null),
            ("onboarding_advance", Value::Null),
            ("card_authorization", json!("card")),
            ("ach_authorization", json!("us_bank_account")),
        ]
    );
    assert!(
        list[5]["text"]
            .as_str()
            .unwrap()
            .contains("debit the bank account I add on this page for: (a) $375.00 per month")
    );
    for consent in list {
        assert_eq!(consent["sha256"], sha(consent["text"].as_str().unwrap()));
    }
    assert!(
        list[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("I am authorized to bind Fresh Stone.")
    );
    assert_eq!(
        list[0]["documents"][0]["url"],
        "https://app.example/terms?v=terms-2026-10-v3"
    );
    assert!(
        list[1]["text"]
            .as_str()
            .unwrap()
            .contains("charged $375.00 per month")
    );
    assert!(
        list[1]["text"]
            .as_str()
            .unwrap()
            .contains("With 12 users that is $435.00 per month")
    );
    let before = list[1]["sha256"].clone();

    let changed = env
        .server
        .put(&format!("/v1/public/signups/{id}/plan"))
        .json(&json!({ "users": 12, "addons": ["quotes", "quickbooks"] }))
        .await;
    changed.assert_status_ok();
    assert_eq!(
        changed.json::<Value>()["quote"]["base_and_addons_cents"],
        40500
    );
    let after: Value = env.server.get(&path).await.json();
    assert_ne!(after["consents"][1]["sha256"], before);
    assert_eq!(after["consents"][0]["sha256"], list[0]["sha256"]);
}

#[test_db::test(seed)]
async fn payment_setup_test_seam(pool: MySqlPool) {
    let env = env(pool).await;
    let id = verified(&env, "pay@fresh.test").await;
    let path = format!("/v1/public/signups/{id}/payment-setup");
    let res = env.server.post(&path).await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>(), json!({ "mode": "test" }));

    let (status, customer, si, attempts): (String, Option<String>, Option<String>, u8) =
        sqlx::query_as(
            "SELECT status, stripe_customer_id, setup_intent_id, setup_attempts
               FROM signup_intents WHERE id = ?",
        )
        .bind(&id)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(status, "payment_ready");
    assert_eq!(attempts, 1);
    let customer = customer.unwrap();
    {
        let stripe = env.stripe.lock().unwrap();
        let created = stripe.calls("POST", "/v1/customers");
        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0].idempotency_key.as_deref(),
            Some(format!("signup:{id}:customer").as_str())
        );
        assert_eq!(created[0].field("email").as_deref(), Some("pay@fresh.test"));
        let intent = &stripe.setup_intents[si.as_deref().unwrap()];
        assert_eq!(intent["status"], "succeeded");
        assert_eq!(intent["customer"], customer);
        let made = stripe.calls("POST", "/v1/setup_intents");
        assert_eq!(
            made[0].idempotency_key.as_deref(),
            Some(format!("signup:{id}:si:1").as_str())
        );
        assert_eq!(
            made[0].field("payment_method").as_deref(),
            Some("pm_card_visa")
        );
    }
    // A second setup reuses the customer.
    env.server.post(&path).await.assert_status_ok();
    assert_eq!(
        env.stripe
            .lock()
            .unwrap()
            .calls("POST", "/v1/customers")
            .len(),
        1
    );
    let signup: Value = env
        .server
        .get(&format!("/v1/public/signups/{id}"))
        .await
        .json();
    assert_eq!(signup["status"], "payment_ready");
}

#[test_db::test(seed)]
async fn payment_setup_needs_a_verified_email(pool: MySqlPool) {
    let env = env(pool).await;
    let id = start(&env, "early@fresh.test").await;
    let res = env
        .server
        .post(&format!("/v1/public/signups/{id}/payment-setup"))
        .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(res.json::<Value>()["code"], "invalid_state");
    assert!(
        env.stripe
            .lock()
            .unwrap()
            .calls("POST", "/v1/customers")
            .is_empty()
    );
}

#[test_db::test(seed)]
async fn payment_setup_in_stripe_mode(pool: MySqlPool) {
    let env = env_with(
        pool,
        SignupConfig {
            enabled: true,
            publishable_key: Some("pk_test_fake".into()),
            ..Default::default()
        },
    )
    .await;
    let id = verified(&env, "card@fresh.test").await;
    let signup: Value = env
        .server
        .get(&format!("/v1/public/signups/{id}"))
        .await
        .json();
    assert_eq!(signup["payment_mode"], "stripe");
    let res = env
        .server
        .post(&format!("/v1/public/signups/{id}/payment-setup"))
        .await;
    res.assert_status_ok();
    let json: Value = res.json();
    assert_eq!(json["mode"], "stripe");
    assert_eq!(json["publishable_key"], "pk_test_fake");
    assert!(json["client_secret"].as_str().unwrap().contains("_secret_"));
    let setup = {
        let stripe = env.stripe.lock().unwrap();
        let made = stripe.calls("POST", "/v1/setup_intents");
        assert_eq!(made[0].field("payment_method"), None);
        assert_eq!(made[0].field("confirm"), None);
        stripe.setup_intents.keys().next().unwrap().clone()
    };
    let path = format!("/v1/public/signups/{id}");
    let signup: Value = env.server.get(&path).await.json();
    assert_eq!(signup["publishable_key"], "pk_test_fake");
    // Not confirmed yet: nothing usable is saved.
    assert_eq!(signup["payment_method"], Value::Null);

    // The browser confirmed a card: a reload shows it saved.
    env.stripe
        .lock()
        .unwrap()
        .confirm_setup_intent(&setup, "pm_card_visa");
    let signup: Value = env.server.get(&path).await.json();
    assert_eq!(
        signup["payment_method"],
        json!({ "status": "ready", "type": "card", "brand": "visa", "last4": "4242",
                "verify_url": null, "verify_by": null })
    );
    // Stripe unreadable: the field is null, the page still loads.
    env.stripe.lock().unwrap().fail(
        axum::http::Method::GET,
        "/v1/setup_intents/",
        fake_stripe::FailMode::Before,
        3,
    );
    let res = env.server.get(&path).await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["payment_method"], Value::Null);
}

/// The test seam answers only with a `sk_test_` key: a live-looking key
/// collects the payment method with the Payment Element, even with a test
/// payment method configured.
#[test_db::test(seed)]
async fn a_live_key_never_takes_the_test_seam(pool: MySqlPool) {
    let stripe = fake_stripe::FakeStripe::with_catalog();
    let sink = Arc::new(Mutex::new(fake_webhooks::Sink::default()));
    let base = fake_stripe::start(Arc::clone(&stripe)).await;
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_stripe(api::billing::stripe::Stripe::new(
            Some("sk_live_fake"),
            &base,
        ))
        .with_webhooks(fake_webhooks::start(&sink).await)
        .with_app_url("https://app.example")
        .with_signup(SignupConfig {
            enabled: true,
            test_payment_method: Some("pm_card_visa".into()),
            publishable_key: Some("pk_live_fake".into()),
            ..Default::default()
        });
    assert!(!api::signup::payment::is_test_mode(&state));
    let env = Env {
        server: TestServer::new(app(state)),
        stripe,
        sink,
        pool,
    };
    let id = verified(&env, "live@fresh.test").await;
    let res = env
        .server
        .post(&format!("/v1/public/signups/{id}/payment-setup"))
        .await;
    res.assert_status_ok();
    let json: Value = res.json();
    assert_eq!(json["mode"], "stripe");
    assert_eq!(json["publishable_key"], "pk_live_fake");
    let stripe = env.stripe.lock().unwrap();
    let made = stripe.calls("POST", "/v1/setup_intents");
    assert_eq!(made.len(), 1);
    assert_eq!(made[0].field("confirm"), None);
    assert_eq!(made[0].field("payment_method"), None);
    assert_eq!(
        stripe.setup_intents.values().next().unwrap()["status"],
        "requires_payment_method"
    );
}

/// Another request holding the email's lock: 503 `signup_busy`, nothing stored.
#[test_db::test(seed)]
async fn parallel_signups_from_one_ip_stop_at_the_limit(pool: MySqlPool) {
    let env = env(pool).await;
    // Different emails, so the per-email lock never serializes them: only
    // the per-IP lock keeps the count and the insert together.
    let requests = (0..14).map(|n| {
        env.server
            .post("/v1/public/signups")
            .add_header("x-forwarded-for", "203.0.113.8")
            .json(&body(&format!("burst{n}@fresh.test")))
    });
    let answers =
        futures_util::future::join_all(requests.map(|request| async move { request.await })).await;
    let created = answers
        .iter()
        .filter(|res| res.status_code() == StatusCode::CREATED)
        .count();
    let limited = answers
        .iter()
        .filter(|res| res.status_code() == StatusCode::TOO_MANY_REQUESTS)
        .count();
    assert_eq!((created, limited), (10, 4));
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM signup_intents WHERE ip = '203.0.113.8'")
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(rows, 10);
}

#[test_db::test(seed)]
async fn a_busy_email_lock_is_signup_busy(pool: MySqlPool) {
    let env = env(pool).await;
    let email = "busy@fresh.test";
    let held =
        api::billing::lock::NamedLock::acquire(&env.pool, &api::signup::email_lock_name(email), 1)
            .await
            .ok()
            .unwrap();
    let res = env
        .server
        .post("/v1/public/signups")
        .json(&body(email))
        .await;
    held.release().await;
    res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    let json: Value = res.json();
    assert_eq!(json["code"], "signup_busy");
    assert_eq!(
        json["error"],
        "We're finishing another request. Try again in a moment."
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM signup_intents")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

/// Prices that cannot be read: 503 `payments_unavailable`, not the kill switch's code.
#[test_db::test(seed)]
async fn unreadable_prices_are_payments_unavailable(pool: MySqlPool) {
    let env = env(pool).await;
    env.stripe.lock().unwrap().fail(
        axum::http::Method::GET,
        "/v1/prices",
        fake_stripe::FailMode::Before,
        10,
    );
    for res in [
        env.server.get("/v1/public/plans").await,
        env.server
            .post("/v1/public/signups")
            .json(&body("prices@fresh.test"))
            .await,
    ] {
        res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.json::<Value>()["code"], "payments_unavailable");
    }
}

#[test_db::test(seed)]
async fn stripe_mode_without_a_publishable_key_is_unavailable(pool: MySqlPool) {
    let env = env_with(
        pool,
        SignupConfig {
            enabled: true,
            ..Default::default()
        },
    )
    .await;
    let id = verified(&env, "nokey@fresh.test").await;
    let res = env
        .server
        .post(&format!("/v1/public/signups/{id}/payment-setup"))
        .await;
    res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(res.json::<Value>()["code"], "payments_unavailable");
}

#[test_db::test(seed)]
async fn the_sixth_payment_setup_is_refused(pool: MySqlPool) {
    let env = env(pool).await;
    let id = verified(&env, "many@fresh.test").await;
    let path = format!("/v1/public/signups/{id}/payment-setup");
    for _ in 0..5 {
        env.server.post(&path).await.assert_status_ok();
    }
    let res = env.server.post(&path).await;
    res.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(res.json::<Value>()["code"], "setup_limited");
    assert_eq!(
        env.stripe
            .lock()
            .unwrap()
            .calls("POST", "/v1/setup_intents")
            .len(),
        5
    );
}

#[test_db::test(seed)]
async fn a_stripe_outage_is_payments_unavailable(pool: MySqlPool) {
    let env = env(pool).await;
    let id = verified(&env, "out@fresh.test").await;
    env.stripe.lock().unwrap().fail(
        axum::http::Method::POST,
        "/v1/customers",
        fake_stripe::FailMode::Before,
        1,
    );
    let res = env
        .server
        .post(&format!("/v1/public/signups/{id}/payment-setup"))
        .await;
    res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(res.json::<Value>()["code"], "payments_unavailable");
    let signup: Value = env
        .server
        .get(&format!("/v1/public/signups/{id}"))
        .await
        .json();
    assert_eq!(signup["status"], "verified");
}

#[test_db::test(seed)]
async fn expired_and_unknown_signups(pool: MySqlPool) {
    let env = env(pool).await;
    let id = start(&env, "gone@fresh.test").await;
    sqlx::query("UPDATE signup_intents SET expires_at = expires_at - INTERVAL 2 DAY WHERE id = ?")
        .bind(&id)
        .execute(&env.pool)
        .await
        .unwrap();
    let res = env.server.get(&format!("/v1/public/signups/{id}")).await;
    res.assert_status(StatusCode::GONE);
    let json: Value = res.json();
    assert_eq!(json["code"], "signup_expired");
    assert_eq!(json["error"], "This signup expired. Start again.");
    verify(&env, &id, "123456")
        .await
        .assert_status(StatusCode::GONE);

    for unknown in ["11111111-1111-4111-8111-111111111111", "not-a-uuid"] {
        let res = env
            .server
            .get(&format!("/v1/public/signups/{unknown}"))
            .await;
        res.assert_status(StatusCode::NOT_FOUND);
        let json: Value = res.json();
        assert_eq!(json["code"], "signup_expired");
        assert_eq!(json["error"], "Signup not found.");
    }
}

#[test_db::test(seed)]
async fn plan_changes_stop_after_provisioning_starts(pool: MySqlPool) {
    let env = env(pool).await;
    let id = verified(&env, "late2@fresh.test").await;
    sqlx::query("UPDATE signup_intents SET status = 'provisioning' WHERE id = ?")
        .bind(&id)
        .execute(&env.pool)
        .await
        .unwrap();
    let res = env
        .server
        .put(&format!("/v1/public/signups/{id}/plan"))
        .json(&json!({ "users": 3, "addons": [] }))
        .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(res.json::<Value>()["code"], "invalid_state");
}

#[test_db::test(seed)]
async fn completion_with_stale_consents_is_refused(pool: MySqlPool) {
    let env = env(pool).await;
    let id = verified(&env, "stale@fresh.test").await;
    env.server
        .post(&format!("/v1/public/signups/{id}/payment-setup"))
        .await
        .assert_status_ok();
    let stale: Vec<Value> = [
        "terms",
        "auto_renewal",
        "per_user_pricing",
        "onboarding_advance",
    ]
    .iter()
    .map(|kind| json!({ "kind": kind, "version": "x-v0", "text_sha256": "0".repeat(64) }))
    .collect();
    let res = env
        .server
        .post(&format!("/v1/public/signups/{id}/completion"))
        .json(&json!({ "consents": stale }))
        .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(res.json::<Value>()["code"], "consents_changed");
}

/// The whole flow over HTTP with the test payment seam: the owner ends up
/// signed in as the Manager and Company Admin of a subscription company.
#[test_db::test(seed)]
async fn a_full_signup_ends_signed_in(pool: MySqlPool) {
    let env = env(pool).await;
    let email = "owner@full-signup.test";
    env.server
        .get("/v1/public/plans?users=12&addon=quotes")
        .await
        .assert_status_ok();
    let id = verified(&env, email).await;
    env.server
        .post(&format!("/v1/public/signups/{id}/payment-setup"))
        .json(&json!({}))
        .await
        .assert_status_ok();
    let consents: Value = env
        .server
        .get(&format!("/v1/public/signups/{id}/consents"))
        .await
        .json();
    let accepted: Vec<Value> = consents["consents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| json!({ "kind": c["kind"], "version": c["version"], "text_sha256": c["sha256"] }))
        .collect();
    let done = env
        .server
        .post(&format!("/v1/public/signups/{id}/completion"))
        .json(&json!({ "consents": accepted }))
        .await;
    done.assert_status_ok();
    assert_eq!(
        done.json::<Value>(),
        json!({
            "redirect_url": "https://app.example/employee?welcome=1",
            "bank_verification": null,
            "signed_in": true
        })
    );
    let cookie = done
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("__session="))
        .and_then(|value| value.split(';').next())
        .expect("a session cookie")
        .to_string();
    let session: Value = env
        .server
        .get("/v1/auth/session")
        .add_header("cookie", cookie)
        .await
        .json();
    assert_eq!(session["user"]["email"], email);
    assert_eq!(session["user"]["is_admin"], true);
    assert_eq!(session["company_subdomain"], Value::Null);

    let (company_id, plan_source, billing_required): (i32, String, bool) = sqlx::query_as(
        "SELECT c.id, CAST(c.plan_source AS CHAR), c.billing_required <> 0
           FROM signup_intents s JOIN company c ON c.id = s.company_id
          WHERE s.id = ? AND s.status = 'completed'",
    )
    .bind(&id)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(
        (plan_source.as_str(), billing_required),
        ("subscription", true)
    );
    let manager: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users u JOIN users_positions up ON up.user_id = u.id
          WHERE u.company_id = ? AND up.position_id = 5 AND u.is_admin = 1 AND u.is_employee = 1",
    )
    .bind(company_id)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(manager, 1);
    let quotes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM company_addons
          WHERE company_id = ? AND addon_key = 'quotes' AND source = 'subscription' AND ended_at IS NULL",
    )
    .bind(company_id)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(quotes, 1);

    // A retry after success: same answer, no new session, nothing created twice.
    let again = env
        .server
        .post(&format!("/v1/public/signups/{id}/completion"))
        .json(&json!({ "consents": [] }))
        .await;
    again.assert_status_ok();
    assert!(again.headers().get("set-cookie").is_none());
    assert_eq!(again.json::<Value>()["signed_in"], false);
    let stripe = env.stripe.lock().unwrap();
    assert_eq!(stripe.calls("POST", "/v1/subscriptions").len(), 1);
    assert_eq!(stripe.subscriptions.len(), 1);
}

/// An extra-user price the consent texts do not state closes signup (503).
#[test_db::test(seed)]
async fn prices_that_differ_from_the_consent_texts_close_signup(pool: MySqlPool) {
    let env = env(pool).await;
    {
        let mut stripe = env.stripe.lock().unwrap();
        let extra = stripe
            .prices
            .iter_mut()
            .find(|price| price["lookup_key"] == "granite_extra_user_monthly_v1")
            .unwrap();
        extra["unit_amount"] = json!(3500);
    }
    let plans = env.server.get("/v1/public/plans").await;
    plans.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(plans.json::<Value>()["code"], "payments_unavailable");
    let create = env
        .server
        .post("/v1/public/signups")
        .json(&body("owner@prices.test"))
        .await;
    create.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM signup_intents")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

/// Sign-in caps passwords at 200 bytes: a longer one is refused at signup.
#[test_db::test(seed)]
async fn passwords_are_capped_in_bytes_like_sign_in(pool: MySqlPool) {
    let env = env(pool).await;
    let mut long = body("owner@bytes.test");
    long["owner"]["password"] = json!("é".repeat(101));
    env.server
        .post("/v1/public/signups")
        .json(&long)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let mut fits = body("owner@bytes.test");
    fits["owner"]["password"] = json!("é".repeat(100));
    env.server
        .post("/v1/public/signups")
        .json(&fits)
        .await
        .assert_status(StatusCode::CREATED);
}
