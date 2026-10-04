mod common;

use api::auth::cookie::{SessionData, encode_session};
use axum::http::StatusCode;
use common::{WithSession, cookie, cookie_for, server, sessions};
use sqlx::MySqlPool;

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn missing_cookie_is_401(pool: MySqlPool) {
    let res = server(pool).get("/v1/me").await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    res.assert_json(&serde_json::json!({ "error": "unauthorized" }));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn cookie_signed_with_another_secret_is_401(pool: MySqlPool) {
    let data = SessionData {
        session_id: Some(sessions::REP.into()),
        active_company_id: None,
    };
    let forged = format!("__session={}", encode_session(&data, "not-the-secret"));
    server(pool)
        .get("/v1/me")
        .add_header("cookie", forged)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn dead_sessions_are_401(pool: MySqlPool) {
    let server = server(pool);
    for session in [
        sessions::EXPIRED,
        sessions::LOGGED_OUT,
        sessions::TOO_OLD,
        sessions::DELETED_USER,
        "cccccccc-0000-4000-8000-000000000000", // unknown
        "not-a-uuid",
    ] {
        let res = server.get("/v1/me").with_session(session).await;
        assert_eq!(
            res.status_code(),
            StatusCode::UNAUTHORIZED,
            "session {session}"
        );
    }
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn session_cookie_among_other_cookies(pool: MySqlPool) {
    let header = format!("theme=dark; {}; _ga=GA1.1", cookie(sessions::REP));
    server(pool)
        .get("/v1/me")
        .add_header("cookie", header)
        .await
        .assert_status_ok();
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn non_employee_can_read_me_but_not_customers(pool: MySqlPool) {
    let server = server(pool);
    server
        .get("/v1/me")
        .with_session(sessions::VIEWER)
        .await
        .assert_status_ok();
    let res = server
        .get("/v1/customers")
        .with_session(sessions::VIEWER)
        .await;
    res.assert_status(StatusCode::FORBIDDEN);
    res.assert_json(&serde_json::json!({ "error": "forbidden" }));
    server
        .post("/v1/customers/emails/batch")
        .with_session(sessions::VIEWER)
        .json(&serde_json::json!({ "customer_ids": [1000] }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn admin_without_employee_flag_is_an_employee(pool: MySqlPool) {
    server(pool)
        .get("/v1/customers")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn super_admin_acts_in_active_company(pool: MySqlPool) {
    let server = server(pool);

    let home: serde_json::Value = server
        .get("/v1/me")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(home["company_id"], 101);
    assert_eq!(home["is_admin"], true);
    assert_eq!(home["is_employee"], true);

    let switched = cookie_for(SessionData {
        session_id: Some(sessions::SUPER_ADMIN.into()),
        active_company_id: Some(100),
    });
    let me: serde_json::Value = server
        .get("/v1/me")
        .add_header("cookie", switched.clone())
        .await
        .json();
    assert_eq!(me["company_id"], 100);
    let customers: serde_json::Value = server
        .get("/v1/customers")
        .add_header("cookie", switched)
        .await
        .json();
    assert!(common::ids(&customers).contains(&1000));

    // A company the user is not super admin in is ignored.
    let not_theirs = cookie_for(SessionData {
        session_id: Some(sessions::SUPER_ADMIN.into()),
        active_company_id: Some(1),
    });
    let me: serde_json::Value = server
        .get("/v1/me")
        .add_header("cookie", not_theirs)
        .await
        .json();
    assert_eq!(me["company_id"], 101);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn regular_user_cannot_switch_company(pool: MySqlPool) {
    let switched = cookie_for(SessionData {
        session_id: Some(sessions::REP.into()),
        active_company_id: Some(101),
    });
    let customers: serde_json::Value = server(pool)
        .get("/v1/customers")
        .add_header("cookie", switched)
        .await
        .json();
    assert!(!common::ids(&customers).contains(&2000));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn cors_allows_credentials_for_known_origins_only(pool: MySqlPool) {
    let server = server(pool);
    let allowed = server
        .method(axum::http::Method::OPTIONS, "/v1/customers/emails/batch")
        .add_header("origin", common::ALLOWED_ORIGIN)
        .add_header("access-control-request-method", "POST")
        .add_header("access-control-request-headers", "content-type")
        .await;
    assert_eq!(
        allowed.header("access-control-allow-origin"),
        common::ALLOWED_ORIGIN
    );
    assert_eq!(allowed.header("access-control-allow-credentials"), "true");

    let denied = server
        .get("/v1/me")
        .add_header("origin", "https://evil.example")
        .with_session(sessions::REP)
        .await;
    assert!(denied.maybe_header("access-control-allow-origin").is_none());
}
