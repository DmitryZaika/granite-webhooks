use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn the_account_is_saved_and_finished_setup_pays_once(pool: MySqlPool) {
    sqlx::query("UPDATE company SET onboarding_enabled = 1 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(pool.clone());
    let account: Value = server
        .get("/v1/me/account")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(account["name"], "Rita Rep");
    assert_eq!(account["email"], "rep@acme.test");
    assert_eq!(account["shows_email_signature"], true);
    assert_eq!(account["has_onboarding_feedback"], false);

    let body = json!({
        "name": " Rita R. ",
        "email": "Rep@Acme.test",
        "phone_number": "(317) 555-0100",
        "password": "",
        "email_name": "Rita",
        "email_signature": "<p><b>Rita</b></p><script>alert(1)</script><img src=x onerror=alert(1)>",
    });
    let saved: Value = server
        .put("/v1/me/account")
        .with_session(sessions::REP)
        .json(&body)
        .await
        .json();
    assert_eq!(saved["texting_line_status"], "none");
    assert!(saved["texting_line_provider"].is_null());
    assert_eq!(
        saved["credit"]["task_ids"],
        json!(["setup-email-name", "setup-email-signature"])
    );
    let (name, email, signature): (String, String, String) =
        sqlx::query_as("SELECT name, email, email_signature FROM users WHERE id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(name, "Rita R.");
    assert_eq!(email, "rep@acme.test");
    assert!(signature.contains("<b>Rita</b>"), "{signature}");
    assert!(!signature.contains("script") && !signature.contains("onerror"));

    // Saving again pays nothing more; fields left out keep their values.
    let again: Value = server
        .put("/v1/me/account")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Rita R.", "email": "rep@acme.test" }))
        .await
        .json();
    assert!(again["credit"].is_null());
    let kept: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT email_name, phone_number FROM users WHERE id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kept.0.as_deref(), Some("Rita"));
    assert_eq!(kept.1.as_deref(), Some("(317) 555-0100"));
}

#[test_db::test(seed)]
async fn a_new_password_ends_the_other_sessions(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .put("/v1/me/account")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Rita Rep", "email": "rep@acme.test", "password": "short" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .put("/v1/me/account")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Rita Rep", "email": "rep@acme.test", "password": " a-new-password " }))
        .await
        .assert_status_ok();
    let hash: String = sqlx::query_scalar("SELECT password FROM users WHERE id = 100")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(bcrypt::verify("a-new-password", &hash).unwrap());
    let ended: Vec<(String, bool)> =
        sqlx::query_as("SELECT id, is_deleted = 1 FROM sessions WHERE user_id = 100 ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    for (id, deleted) in ended {
        assert_eq!(deleted, id != sessions::REP, "{id}");
    }
    server
        .get("/v1/me/account")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn invalid_saves_are_refused(pool: MySqlPool) {
    let server = server(pool.clone());
    for (body, status) in [
        (
            json!({ "name": "Rita", "email": "manager@acme.test" }),
            StatusCode::CONFLICT,
        ),
        (
            json!({ "name": " ", "email": "rep@acme.test" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "name": "Rita", "email": "not-an-email" }),
            StatusCode::BAD_REQUEST,
        ),
        // Patterns and address lists: mail is matched to users by address.
        (
            json!({ "name": "Rita", "email": "%@%.%" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "name": "Rita", "email": "rep%@acme.test" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "name": "Rita", "email": "Rita <rep@acme.test>" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "name": "Rita", "email": "rep@acme.test, boss@acme.test" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "name": "Rita", "email": "rep@acme_test.com" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "name": "Rita", "email": "rep@acme.test", "cloudtalk_phone_number": "55512" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "name": "Rita", "email": "rep@acme.test", "cloudtalk_agent_id": "12a" }),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        server
            .put("/v1/me/account")
            .with_session(sessions::REP)
            .json(&body)
            .await
            .assert_status(status);
    }
    let email: String = sqlx::query_scalar("SELECT email FROM users WHERE id = 100")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(email, "rep@acme.test");
}

#[test_db::test(seed)]
async fn a_cloudtalk_line_needs_its_agent(pool: MySqlPool) {
    let server = server(pool.clone());
    let incomplete: Value = server
        .put("/v1/me/account")
        .with_session(sessions::REP)
        .json(&json!({
            "name": "Rita Rep", "email": "rep@acme.test",
            "cloudtalk_agent_id": "", "cloudtalk_phone_number": "13175550100",
        }))
        .await
        .json();
    assert_eq!(incomplete["texting_line_status"], "incomplete");
    assert_eq!(incomplete["texting_line_provider"], "CloudTalk");
    let line: Option<String> =
        sqlx::query_scalar("SELECT cloudtalk_phone_number FROM users WHERE id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(line.as_deref(), Some("3175550100"));
}
