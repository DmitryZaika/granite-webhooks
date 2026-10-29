use crate::common;

use ::common::telephony::ringcentral_auth::decrypt;
use api::app;
use api::qbo::Qbo;
use api::ringcentral_connect::RingCentralApp;
use api::state::AppState;
use api::storage::Storage;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, png, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const KEY: [u8; 32] = [7; 32];
const APP_URL: &str = "https://granite-manager.test";

type Seen = Arc<Mutex<Vec<String>>>;

/// Token endpoints of Intuit and RingCentral: they record the form and
/// answer tokens for `code=good`, 400 otherwise.
async fn token_stub() -> (String, Seen) {
    let seen: Seen = Arc::default();
    let qbo_seen = Arc::clone(&seen);
    let rc_seen = Arc::clone(&seen);
    let router = Router::new()
        .route(
            "/token",
            post(move |body: String| async move {
                qbo_seen.lock().unwrap().push(body.clone());
                if body.contains("code=good") {
                    (
                        StatusCode::OK,
                        Json(
                            json!({ "access_token": "qbo-access", "refresh_token": "qbo-refresh",
                                     "expires_in": 3600, "x_refresh_token_expires_in": 8_640_000 }),
                        ),
                    )
                } else {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "invalid_grant" })),
                    )
                }
            }),
        )
        .route(
            "/restapi/oauth/token",
            post(move |body: String| async move {
                rc_seen.lock().unwrap().push(body.clone());
                if body.contains("code=good") {
                    (
                        StatusCode::OK,
                        Json(
                            json!({ "access_token": "rc-access", "refresh_token": "rc-refresh",
                                     "expires_in": 3600, "refresh_token_expires_in": 604_800,
                                     "owner_id": "acct-9" }),
                        ),
                    )
                } else {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "invalid_grant" })),
                    )
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, seen)
}

struct Setup {
    server: TestServer,
    storage: Storage,
    seen: Seen,
}

async fn setup(pool: &MySqlPool, shared_qbo_app: bool, ringcentral: bool) -> Setup {
    let (base, seen) = token_stub().await;
    let shared = shared_qbo_app.then_some(("shared-client-id", "shared-client-secret"));
    let qbo = Qbo::new(Some(KEY), shared, &base, &format!("{base}/token"), false);
    let storage = Storage::memory();
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_qbo(qbo)
        .with_storage(storage.clone())
        .with_app_url(APP_URL)
        .with_ringcentral_app(
            ringcentral
                .then(|| RingCentralApp::new("rc-client", "rc-secret", &base, Some(KEY)))
                .flatten(),
        );
    Setup {
        server: TestServer::new(app(state)),
        storage,
        seen,
    }
}

/// Ada Admin (102) holds the Manager position of company 100.
async fn admin_manages(pool: &MySqlPool) {
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (102, 5, 100)",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn query(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

// ---- Company settings

#[test_db::test(seed)]
async fn managers_read_and_save_the_company_settings(pool: MySqlPool) {
    admin_manages(&pool).await;
    sqlx::query(
        "UPDATE company SET address = '1 Main St', logo_url = 'https://cdn.example/logo.png',
                logo_height = 64, state_taxes = 0.0725 WHERE id = 100",
    )
    .execute(&pool)
    .await
    .unwrap();
    let Setup {
        server, storage, ..
    } = setup(&pool, false, false).await;
    let settings: Value = server
        .get("/v1/company/settings")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(
        settings,
        json!({
            "company_id": 100,
            "name": "Acme Stone",
            "address": "1 Main St",
            "logo_url": "https://cdn.example/logo.png",
            "logo_height": 64,
            "sales_tax_percent": 7.25,
            "can_request_api_token": true,
            "quickbooks": { "feature_enabled": true, "credentials_source": "none", "connected": false },
            "ringcentral": { "connect_available": false, "status": null }
        })
    );

    // A new logo is an upload to company-logos.
    let upload: Value = server
        .post("/v1/uploads")
        .with_session(sessions::ADMIN)
        .json(&json!({ "folder": "company-logos", "file_name": "logo.png", "content_type": "image/png" }))
        .await
        .json();
    let logo = upload["url"].as_str().unwrap().to_string();
    storage
        .put(
            &storage.key_for_url(&logo).unwrap(),
            png(120, 40),
            "image/png",
        )
        .await
        .unwrap();
    server
        .put("/v1/company/settings")
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "name": " Acme Stone Co ", "address": "", "logo_url": logo,
                       "logo_height": 96, "sales_tax_percent": 6.5 }),
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let saved: (String, Option<String>, Option<String>, i32, String) = sqlx::query_as(
        "SELECT name, address, logo_url, logo_height, CAST(state_taxes AS CHAR) FROM company WHERE id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        saved,
        (
            "Acme Stone Co".into(),
            None,
            Some(logo),
            96,
            "0.0650".into()
        )
    );

    // Without a new logo the old one stays; a null tax clears it.
    server
        .put("/v1/company/settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Acme", "logo_height": 48, "sales_tax_percent": null }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let settings: Value = server
        .get("/v1/company/settings")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert!(
        settings["logo_url"]
            .as_str()
            .unwrap()
            .contains("company-logos/")
    );
    assert_eq!(settings["sales_tax_percent"], Value::Null);
}

#[test_db::test(seed)]
async fn company_settings_are_checked_and_scoped(pool: MySqlPool) {
    admin_manages(&pool).await;
    let Setup { server, .. } = setup(&pool, false, false).await;
    for body in [
        json!({ "name": " ", "logo_height": 48 }),
        json!({ "name": "A", "logo_height": 500 }),
        json!({ "name": "A", "logo_height": 48, "sales_tax_percent": 101 }),
        json!({ "name": "A", "logo_height": 48, "logo_url": "https://elsewhere.example/x.png" }),
    ] {
        server
            .put("/v1/company/settings")
            .with_session(sessions::ADMIN)
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    server
        .get("/v1/company/settings")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // An employee, and an admin who is not a Manager.
    server
        .get("/v1/company/settings")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    sqlx::query("DELETE FROM users_positions WHERE user_id = 102 AND position_id = 5")
        .execute(&pool)
        .await
        .unwrap();
    server
        .get("/v1/company/settings")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn only_superusers_pick_another_company(pool: MySqlPool) {
    admin_manages(&pool).await;
    let Setup { server, .. } = setup(&pool, false, false).await;
    // A manager's company_id is ignored.
    let own: Value = server
        .get("/v1/company/settings?company_id=101")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(own["company_id"], 100);
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(&pool)
        .await
        .unwrap();
    let other: Value = server
        .get("/v1/company/settings?company_id=101")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(other["company_id"], 101);
    assert_eq!(other["can_request_api_token"], false);
}

#[test_db::test(seed)]
async fn the_api_token_email_needs_a_manager_and_a_token(pool: MySqlPool) {
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(&pool)
        .await
        .unwrap();
    let Setup { server, .. } = setup(&pool, false, false).await;
    // A superuser without the Manager position.
    server
        .post("/v1/company/api-token-email")
        .with_session(sessions::ADMIN)
        .json(&json!({ "email": "ada@acme.test" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    admin_manages(&pool).await;
    server
        .post("/v1/company/api-token-email")
        .with_session(sessions::ADMIN)
        .json(&json!({ "email": "not an email" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    // The test environment has no GRANITE_MANAGER_DOCS_BEARER_TOKEN.
    server
        .post("/v1/company/api-token-email")
        .with_session(sessions::ADMIN)
        .json(&json!({ "email": "ada@acme.test" }))
        .await
        .assert_status(StatusCode::SERVICE_UNAVAILABLE);
}

// ---- QuickBooks

#[test_db::test(seed)]
async fn quickbooks_connects_through_the_shared_app(pool: MySqlPool) {
    admin_manages(&pool).await;
    let Setup { server, seen, .. } = setup(&pool, true, false).await;
    let status: Value = server
        .get("/v1/quickbooks/connection")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(status, json!({ "can_connect": true, "connected": false }));

    let link: Value = server
        .post("/v1/quickbooks/connection/start")
        .with_session(sessions::REP)
        .json(&json!({}))
        .await
        .json();
    let url = link["url"].as_str().unwrap();
    assert!(url.starts_with("https://appcenter.intuit.com/connect/oauth2?"));
    let params = query(url);
    assert_eq!(params["client_id"], "shared-client-id");
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["scope"], "com.intuit.quickbooks.accounting");
    assert_eq!(
        params["redirect_uri"],
        format!("{APP_URL}/redirects/quickbooks/user-auth")
    );
    let state = params["state"].clone();

    // Another user cannot use this user's state; nor can a forged one.
    for (session, state) in [
        (sessions::DEALER, state.as_str()),
        (sessions::REP, "quickbooks.100.100.9999999999.x.y"),
    ] {
        server
            .post("/v1/quickbooks/connection")
            .with_session(session)
            .json(&json!({ "state": state, "code": "good", "realm_id": "555" }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    server
        .post("/v1/quickbooks/connection")
        .with_session(sessions::REP)
        .json(&json!({ "state": state, "code": "good", "realm_id": "not-a-realm" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/quickbooks/connection")
        .with_session(sessions::REP)
        .json(&json!({ "state": state, "code": "bad", "realm_id": "555" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    let connected: Value = server
        .post("/v1/quickbooks/connection")
        .with_session(sessions::REP)
        .json(&json!({ "state": state, "code": "good", "realm_id": "555" }))
        .await
        .json();
    assert_eq!(connected, json!({ "company_subdomain": "acme" }));
    let exchange = seen.lock().unwrap().last().unwrap().clone();
    assert!(exchange.contains("grant_type=authorization_code"));
    assert!(exchange.contains(
        "redirect_uri=https%3A%2F%2Fgranite-manager.test%2Fredirects%2Fquickbooks%2Fuser-auth"
    ));
    let (realm, access, by): (Option<String>, Vec<u8>, Option<i32>) = sqlx::query_as(
        "SELECT qbo_realm_id, qbo_access_token, qbo_connected_by FROM company WHERE id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(realm.as_deref(), Some("555"));
    assert_eq!(decrypt(&KEY, &access).as_deref(), Some("qbo-access"));
    assert_eq!(by, Some(100));

    let settings: Value = server
        .get("/v1/company/settings")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(
        settings["quickbooks"],
        json!({ "feature_enabled": true, "credentials_source": "shared-app", "connected": true })
    );

    server
        .delete("/v1/quickbooks/connection")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .delete("/v1/quickbooks/connection")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let status: Value = server
        .get("/v1/quickbooks/connection")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(status["connected"], false);
}

#[test_db::test(seed)]
async fn quickbooks_cannot_connect_without_an_app_or_the_feature(pool: MySqlPool) {
    let Setup { server, .. } = setup(&pool, false, false).await;
    server
        .post("/v1/quickbooks/connection/start")
        .with_session(sessions::REP)
        .json(&json!({}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let Setup { server, .. } = setup(&pool, true, false).await;
    sqlx::query("UPDATE company SET quickbooks_enabled = 0 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let status: Value = server
        .get("/v1/quickbooks/connection")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(status, json!({ "can_connect": false, "connected": false }));
}

#[test_db::test(seed)]
async fn intuits_disconnect_link_clears_the_realm(pool: MySqlPool) {
    sqlx::query("UPDATE company SET qbo_realm_id = '777', qbo_access_token = 'x' WHERE id = 101")
        .execute(&pool)
        .await
        .unwrap();
    let Setup { server, .. } = setup(&pool, false, false).await;
    // Knowing the realm id is not enough: signed out, nothing happens.
    server
        .post("/v1/quickbooks/disconnected")
        .json(&json!({ "realm_id": "777" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    for (session, realm) in [
        (sessions::OTHER_COMPANY_REP, "abc"),
        (sessions::OTHER_COMPANY_REP, "' OR 1=1 --"),
        (sessions::OTHER_COMPANY_REP, ""),
        // Another company's user cannot disconnect it either.
        (sessions::REP, "777"),
    ] {
        server
            .post("/v1/quickbooks/disconnected")
            .with_session(session)
            .json(&json!({ "realm_id": realm }))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    let realm: Option<String> =
        sqlx::query_scalar("SELECT qbo_realm_id FROM company WHERE id = 101")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(realm.as_deref(), Some("777"));
    server
        .post("/v1/quickbooks/disconnected")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "realm_id": "777" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let (realm, token): (Option<String>, Option<Vec<u8>>) =
        sqlx::query_as("SELECT qbo_realm_id, qbo_access_token FROM company WHERE id = 101")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((realm, token), (None, None));
}

#[test_db::test(seed)]
async fn superusers_save_a_companys_own_quickbooks_app(pool: MySqlPool) {
    admin_manages(&pool).await;
    let Setup { server, .. } = setup(&pool, false, false).await;
    let body = json!({ "client_id": "company-client-1", "client_secret": "company-secret-1" });
    server
        .put("/v1/superuser/companies/101/quickbooks-app")
        .with_session(sessions::ADMIN)
        .json(&body)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(&pool)
        .await
        .unwrap();
    server
        .put("/v1/superuser/companies/101/quickbooks-app")
        .with_session(sessions::ADMIN)
        .json(&json!({ "client_id": "short", "client_secret": "company-secret-1" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .put("/v1/superuser/companies/101/quickbooks-app")
        .with_session(sessions::ADMIN)
        .json(&body)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let id: Vec<u8> = sqlx::query_scalar("SELECT qbo_client_id FROM company WHERE id = 101")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(decrypt(&KEY, &id).as_deref(), Some("company-client-1"));
    let other: Value = server
        .get("/v1/company/settings?company_id=101")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(other["quickbooks"]["credentials_source"], "company-app");
    server
        .put("/v1/superuser/companies/99999/quickbooks-app")
        .with_session(sessions::ADMIN)
        .json(&body)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

// ---- RingCentral

#[test_db::test(seed)]
async fn ringcentral_connects_and_disconnects(pool: MySqlPool) {
    admin_manages(&pool).await;
    let Setup { server, seen, .. } = setup(&pool, false, true).await;
    let link: Value = server
        .post("/v1/ringcentral/connection/start")
        .with_session(sessions::REP)
        .json(&json!({}))
        .await
        .json();
    let url = link["url"].as_str().unwrap();
    assert!(url.contains("/restapi/oauth/authorize?"));
    let params = query(url);
    assert_eq!(params["client_id"], "rc-client");
    assert_eq!(
        params["redirect_uri"],
        format!("{APP_URL}/redirects/ringcentral/user-auth")
    );
    let state = params["state"].clone();

    server
        .post("/v1/ringcentral/connection")
        .with_session(sessions::REP)
        .json(&json!({ "state": state, "error": "access_denied" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/ringcentral/connection")
        .with_session(sessions::REP)
        .json(&json!({ "state": "nope", "code": "good" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/ringcentral/connection")
        .with_session(sessions::REP)
        .json(&json!({ "state": state, "code": "bad" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/ringcentral/connection")
        .with_session(sessions::REP)
        .json(&json!({ "state": state, "code": "good" }))
        .await
        .assert_status(StatusCode::OK);
    assert!(
        seen.lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("grant_type=authorization_code")
    );
    let (access, account, by): (Vec<u8>, Option<String>, Option<i32>) = sqlx::query_as(
        "SELECT ringcentral_access_token, ringcentral_account_id, ringcentral_connected_by
           FROM company WHERE id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(decrypt(&KEY, &access).as_deref(), Some("rc-access"));
    assert_eq!(account.as_deref(), Some("acct-9"));
    assert_eq!(by, Some(100));

    server
        .delete("/v1/ringcentral/connection")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let left: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT ringcentral_access_token FROM company WHERE id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, None);
}

#[test_db::test(seed)]
async fn ringcentral_connect_needs_the_app(pool: MySqlPool) {
    let Setup { server, .. } = setup(&pool, false, false).await;
    server
        .post("/v1/ringcentral/connection/start")
        .with_session(sessions::REP)
        .json(&json!({}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/ringcentral/connection/start")
        .json(&json!({}))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn managers_switch_the_sale_form_on_a_win(pool: MySqlPool) {
    let Setup { server, .. } = setup(&pool, false, false).await;
    let auto_sale = || async {
        let on: bool = sqlx::query_scalar("SELECT auto_sale <> 0 FROM company WHERE id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
        on
    };
    // Off by default; a rep and a non-manager admin may not switch it.
    assert!(!auto_sale().await);
    for session in [sessions::REP, sessions::ADMIN] {
        server
            .put("/v1/company/auto-sale")
            .with_session(session)
            .json(&json!({ "enabled": true }))
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    admin_manages(&pool).await;
    server
        .put("/v1/company/auto-sale")
        .with_session(sessions::ADMIN)
        .json(&json!({ "enabled": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(auto_sale().await);
    let features: Value = server
        .get("/v1/company/features")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(features["auto_sale"], true);
}
