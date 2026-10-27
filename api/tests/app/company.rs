use crate::common;

use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: company 100 has the leaderboard on and nothing else set up; company
// 101 has a mail domain and CloudTalk credentials.

async fn features(pool: MySqlPool, session: &str) -> Value {
    server(pool)
        .get("/v1/company/features")
        .with_session(session)
        .await
        .json()
}

async fn run(pool: &MySqlPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

#[test_db::test(seed)]
async fn features_of_each_company(pool: MySqlPool) {
    assert_eq!(
        features(pool.clone(), sessions::REP).await,
        json!({
            "quickbooks_enabled": true,
            "quickbooks_connected": false,
            "onboarding": false,
            "leaderboard": true,
            "e_signature": false,
            "quotes": true,
            "email_signature": true,
            "inbound_email": false,
            "cloudtalk": false,
            "ringcentral": false,
            "telnyx": false,
            "telnyx_sms": false
        })
    );
    assert_eq!(
        features(pool, sessions::OTHER_COMPANY_REP).await,
        json!({
            "quickbooks_enabled": true,
            "quickbooks_connected": false,
            "onboarding": false,
            "leaderboard": false,
            "e_signature": false,
            "quotes": true,
            "email_signature": true,
            "inbound_email": true,
            "cloudtalk": true,
            "ringcentral": false,
            "telnyx": false,
            "telnyx_sms": false
        })
    );
}

#[test_db::test(seed)]
async fn rollout_switches_fall_back_to_the_platform_default(pool: MySqlPool) {
    // Company 100: leaderboard explicitly on, onboarding unset.
    run(
        &pool,
        "UPDATE global_feature_flags SET enabled_for_all = 1 WHERE feature_key = 'onboarding';
         UPDATE global_feature_flags SET enabled_for_all = 1 WHERE feature_key = 'leaderboard';
         UPDATE company SET leaderboard_enabled = 0 WHERE id = 101;",
    )
    .await;
    let acme = features(pool.clone(), sessions::REP).await;
    assert_eq!(
        (&acme["onboarding"], &acme["leaderboard"]),
        (&json!(true), &json!(true))
    );
    // An explicit off beats the platform default.
    let other = features(pool, sessions::OTHER_COMPANY_REP).await;
    assert_eq!(
        (&other["onboarding"], &other["leaderboard"]),
        (&json!(true), &json!(false))
    );
}

#[test_db::test(seed)]
async fn integrations_count_once_set_up(pool: MySqlPool) {
    run(
        &pool,
        "UPDATE company SET quickbooks_enabled = 0, qbo_realm_id = ' 123 ',
                qbo_access_token = 'a', qbo_refresh_token = 'r',
                qbo_access_expires = 1, qbo_refresh_expires = 2,
                signwell_api_key = 'key', domain = 'https://www.Acme.test/',
                ringcentral_access_token = 'a', ringcentral_refresh_token = 'r',
                ringcentral_access_expires = 1,
                ringcentral_refresh_expires = UNIX_TIMESTAMP() + 3600
          WHERE id = 100;
         UPDATE company SET cloudtalk_access_secret = '  ', domain = 'not a domain',
                ringcentral_access_token = 'a', ringcentral_refresh_token = 'r',
                ringcentral_access_expires = 1,
                ringcentral_refresh_expires = UNIX_TIMESTAMP() - 1
          WHERE id = 101;
         INSERT INTO telnyx_company_config (company_id, enabled, webhook_token)
         VALUES (100, 1, REPEAT('a', 32)), (101, 0, REPEAT('b', 32));
         -- eSignatures are in use once one user has access (no company switch).
         UPDATE users SET is_signature_available = 1 WHERE id = 100;
         UPDATE company SET signature_enabled = 1 WHERE id = 101;",
    )
    .await;
    assert_eq!(
        features(pool.clone(), sessions::REP).await,
        json!({
            "quickbooks_enabled": false,
            "quickbooks_connected": true,
            "onboarding": false,
            "leaderboard": true,
            "e_signature": true,
            "quotes": true,
            "email_signature": true,
            "inbound_email": true,
            "cloudtalk": false,
            "ringcentral": true,
            "telnyx": true,
            "telnyx_sms": false
        })
    );
    // A blank secret is no CloudTalk; an expired RingCentral refresh token is
    // no connection; the old company eSignatures switch alone is nothing.
    let other = features(pool.clone(), sessions::OTHER_COMPANY_REP).await;
    assert_eq!(
        (
            &other["inbound_email"],
            &other["cloudtalk"],
            &other["ringcentral"],
            &other["telnyx"],
            &other["e_signature"]
        ),
        (
            &json!(false),
            &json!(false),
            &json!(false),
            &json!(false),
            &json!(false)
        )
    );

    // QuickBooks needs every token column, not just the realm.
    run(
        &pool,
        "UPDATE company SET qbo_refresh_token = NULL WHERE id = 100;",
    )
    .await;
    assert_eq!(
        features(pool, sessions::REP).await["quickbooks_connected"],
        false
    );
}

#[test_db::test(seed)]
async fn features_need_an_employee(pool: MySqlPool) {
    let server = server(pool);
    server
        .get("/v1/company/features")
        .expect_failure()
        .await
        .assert_status_unauthorized();
    server
        .get("/v1/company/features")
        .with_session(sessions::VIEWER)
        .expect_failure()
        .await
        .assert_status_forbidden();
}

#[test_db::test(seed)]
async fn the_company_tax_rate_reads_the_state_tax(pool: MySqlPool) {
    let server = server(pool.clone());
    let body: serde_json::Value = server
        .get("/v1/company/tax-rate")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(body["state_tax_pct"], 0.0);
    sqlx::query("UPDATE company SET state_taxes = 7.25 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let body: serde_json::Value = server
        .get("/v1/company/tax-rate")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(body["state_tax_pct"], 7.25);
}

#[test_db::test(seed)]
async fn teach_progress_lists_reps_and_answers(pool: MySqlPool) {
    sqlx::query("INSERT INTO questions (id, text, question_type, company_id) VALUES (900, 'What is granite?', 'MC', 100)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO answer_choices (id, question_id, text, is_correct) VALUES
           (901, 900, 'A stone', 1), (902, 900, 'A wood', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO answer_attempts (employee_id, question_id, selected_answer_id, attempt_number, is_correct)
         VALUES (100, 900, 902, 1, 0), (100, 900, 901, 2, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let body: serde_json::Value = server(pool)
        .get("/v1/teach-mode/progress")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert!(
        body["users"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["id"] == 100)
    );
    let attempts = body["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["attempt_number"], 2);
    assert_eq!(attempts[0]["correct_answer_text"], "A stone");
    assert_eq!(attempts[1]["selected_answer_text"], "A wood");
}
