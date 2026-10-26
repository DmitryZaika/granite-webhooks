//! Signing phones out when a user's access changes elsewhere: a password
//! reset or change, and an admin deleting a user or taking their employee
//! and admin rights away (`phone::revoke` from `routes/auth.rs`,
//! `routes/me/account.rs`, `routes/users/manage.rs`).
//!
//! Fixture phones use n = 151..=170.

use crate::common;
use crate::phone_common;

use api::auth::device::token_hash;
use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, sessions};
use phone_common::{HEADER, Stubs, add_phone, enable_calling, phone_server, sql, token};
use serde_json::json;
use sqlx::MySqlPool;

/// Whether phone `n`'s device token still signs in.
async fn signed_in(server: &TestServer, n: u32) -> bool {
    let status = server
        .get("/v1/phone/bootstrap")
        .add_header(HEADER, token(n))
        .await
        .status_code();
    assert!(
        status == StatusCode::OK || status == StatusCode::UNAUTHORIZED,
        "{status}"
    );
    status == StatusCode::OK
}

/// Why phone `n` (device session and phone device) was signed out, and
/// whether its Telnyx credential is gone.
async fn revoked(pool: &MySqlPool, n: u32) -> (Option<String>, Option<String>, bool) {
    sqlx::query_as(
        "SELECT ds.revoked_reason, pd.revoke_reason, pd.credential_deleted_at IS NOT NULL
           FROM device_sessions ds JOIN phone_devices pd ON pd.device_session_id = ds.id
          WHERE ds.id = ?",
    )
    .bind(950_000 + i64::from(n))
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The Telnyx credentials deleted and the phones told to wipe themselves.
fn signed_out_phones(stubs: &Stubs) -> (Vec<String>, Vec<String>) {
    let deleted = stubs
        .telnyx_paths()
        .into_iter()
        .filter_map(|path| {
            path.strip_prefix("DELETE /telephony_credentials/")
                .map(str::to_string)
        })
        .collect();
    let told = stubs
        .pushes()
        .iter()
        .filter(|push| push["data"]["type"] == "revoked")
        .map(|push| push["token"].as_str().unwrap().to_string())
        .collect();
    (deleted, told)
}

#[test_db::test(seed)]
async fn a_password_reset_signs_the_users_phones_out(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    add_phone(&pool, 151, 100, 100).await;
    add_phone(&pool, 152, 100, 100).await;
    add_phone(&pool, 153, 101, 100).await;
    let link = "ab".repeat(48);
    sqlx::query(
        "INSERT INTO password_reset_tokens (user_id, token_hash, expires_at)
         VALUES (100, ?, NOW() + INTERVAL 1 HOUR)",
    )
    .bind(token_hash(&link))
    .execute(&pool)
    .await
    .unwrap();
    let (server, stubs) = phone_server(pool.clone()).await;
    assert!(signed_in(&server, 151).await);
    server
        .post(&format!("/v1/auth/password-reset/{link}"))
        .json(&json!({ "password": "a-new-password" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // Both of Rita's phones are out, with their credentials; Mark's stays.
    assert!(!signed_in(&server, 151).await);
    assert!(!signed_in(&server, 152).await);
    assert!(signed_in(&server, 153).await);
    for n in [151, 152] {
        assert_eq!(
            revoked(&pool, n).await,
            (
                Some("password_changed".into()),
                Some("password_changed".into()),
                true
            )
        );
    }
    assert_eq!(revoked(&pool, 153).await, (None, None, false));
    assert_eq!(
        signed_out_phones(&stubs),
        (
            vec!["cred-151".to_string(), "cred-152".to_string()],
            vec!["push-151".to_string(), "push-152".to_string()]
        )
    );
}

#[test_db::test(seed)]
async fn a_password_change_signs_the_users_phones_out(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    add_phone(&pool, 154, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    // Saving the account without a new password keeps the phone.
    server
        .put("/v1/me/account")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Rita Rep", "email": "rep@acme.test" }))
        .await
        .assert_status_ok();
    assert!(signed_in(&server, 154).await);
    server
        .put("/v1/me/account")
        .with_session(sessions::REP)
        .json(
            &json!({ "name": "Rita Rep", "email": "rep@acme.test", "password": "a-new-password" }),
        )
        .await
        .assert_status_ok();
    assert!(!signed_in(&server, 154).await);
    assert_eq!(
        revoked(&pool, 154).await,
        (
            Some("password_changed".into()),
            Some("password_changed".into()),
            true
        )
    );
    assert_eq!(
        signed_out_phones(&stubs),
        (vec!["cred-154".to_string()], vec!["push-154".to_string()])
    );
}

#[test_db::test(seed)]
async fn an_admin_deleting_a_user_signs_their_phones_out(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    // Ada manages users once she is a Manager (position 5) in company 100.
    sql(
        &pool,
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (102, 5, 100)",
    )
    .await;
    add_phone(&pool, 155, 101, 100).await;
    add_phone(&pool, 156, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    server
        .delete("/v1/admin/users/101")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(!signed_in(&server, 155).await);
    assert!(signed_in(&server, 156).await);
    assert_eq!(
        revoked(&pool, 155).await,
        (
            Some("user_removed".into()),
            Some("user_removed".into()),
            true
        )
    );
    assert_eq!(
        signed_out_phones(&stubs),
        (vec!["cred-155".to_string()], vec!["push-155".to_string()])
    );
}

#[test_db::test(seed)]
async fn deleting_a_company_stops_its_calling_and_signs_its_phones_out(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    enable_calling(&pool, 101, "").await;
    sql(&pool, "UPDATE users SET is_superuser = 1 WHERE id = 103").await;
    add_phone(&pool, 158, 100, 100).await;
    add_phone(&pool, 159, 101, 100).await;
    add_phone(&pool, 160, 106, 101).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    server
        .delete("/v1/superuser/companies/100?confirm_name=Acme%20Stone")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let enabled: Vec<bool> = sqlx::query_scalar(
        "SELECT enabled <> 0 FROM telnyx_company_config WHERE company_id IN (100, 101) ORDER BY company_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(enabled, [false, true]);
    // Company 100's phones are out, with their credentials; 101's stays.
    assert!(!signed_in(&server, 158).await);
    assert!(!signed_in(&server, 159).await);
    assert!(signed_in(&server, 160).await);
    for n in [158, 159] {
        assert_eq!(
            revoked(&pool, n).await,
            (
                Some("company_deleted".into()),
                Some("company_deleted".into()),
                true
            )
        );
    }
    assert_eq!(revoked(&pool, 160).await, (None, None, false));
    assert_eq!(
        signed_out_phones(&stubs),
        (
            vec!["cred-158".to_string(), "cred-159".to_string()],
            vec!["push-158".to_string(), "push-159".to_string()]
        )
    );
}

#[test_db::test(seed)]
async fn losing_employee_and_admin_rights_signs_the_phones_out(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    sql(
        &pool,
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (102, 5, 100)",
    )
    .await;
    add_phone(&pool, 157, 107, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    // A rename keeps Dina an employee: her phone stays.
    server
        .put("/v1/admin/users/107")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Dina D.", "email": "dealer@acme.test", "company_id": 100 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(signed_in(&server, 157).await);
    assert!(stubs.telnyx().is_empty());
    // Only a shop worker now: no longer an employee, so signed out.
    server
        .put("/v1/admin/users/107")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Dina D.", "email": "dealer@acme.test", "company_id": 100, "positions": [4] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(!signed_in(&server, 157).await);
    assert_eq!(
        revoked(&pool, 157).await,
        (
            Some("user_removed".into()),
            Some("user_removed".into()),
            true
        )
    );
    assert_eq!(
        signed_out_phones(&stubs),
        (vec!["cred-157".to_string()], vec!["push-157".to_string()])
    );
}
