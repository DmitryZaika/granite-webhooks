mod common;

use api::auth::cookie::SessionData;
use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, cookie_for, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const NEW_KEYS: [&str; 2] = ["quotes", "email_signature"];

async fn shell(server: &TestServer, session: &str) -> Value {
    server
        .get("/v1/app/shell")
        .with_session(session)
        .await
        .json()
}

/// `value` without the keys this change added.
fn without_new_keys(value: &Value) -> Value {
    let mut old = value.clone();
    for key in NEW_KEYS {
        old.as_object_mut().unwrap().remove(key);
    }
    old
}

async fn make_superuser(pool: &MySqlPool) {
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(pool)
        .await
        .unwrap();
}

async fn grant(server: &TestServer, company: i32, key: &str, enabled: bool) {
    server
        .put(&format!("/v1/superuser/companies/{company}/addons/{key}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "enabled": enabled }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

#[test_db::test(seed)]
async fn legacy_companies_keep_their_features_and_gain_the_new_keys(pool: MySqlPool) {
    let server = server(pool);
    // Captured before the add-on layer existed.
    let old_shell_features = |leaderboard: bool| json!({ "leaderboard": leaderboard, "onboarding": false, "quickbooks": true, "signature": false });
    let old_features = |cloudtalk: bool, inbound_email: bool, leaderboard: bool| {
        json!({
            "cloudtalk": cloudtalk, "e_signature": false, "inbound_email": inbound_email,
            "leaderboard": leaderboard, "onboarding": false, "quickbooks_connected": false,
            "quickbooks_enabled": true, "ringcentral": false,
        })
    };
    for (session, leaderboard, cloudtalk, inbound_email) in [
        (sessions::REP, true, false, false),
        (sessions::MANAGER, true, false, false),
        (sessions::SUPER_ADMIN, false, true, true),
    ] {
        let shell = shell(&server, session).await;
        let new = &shell["company_features"];
        assert_eq!(*new, {
            let mut expected = old_shell_features(leaderboard);
            expected["quotes"] = json!(true);
            expected["email_signature"] = json!(true);
            expected
        });
        assert_eq!(without_new_keys(new), old_shell_features(leaderboard));
        assert_eq!(shell["user_signature_access"], false);
        assert_eq!(
            shell["plan"],
            json!({
                "source": "legacy",
                "addons": ["signatures", "quotes", "quickbooks"],
                "manage_url": "https://payments.granite-manager.com/account",
            })
        );
        let features: Value = server
            .get("/v1/company/features")
            .with_session(session)
            .await
            .json();
        assert_eq!(
            without_new_keys(&features),
            old_features(cloudtalk, inbound_email, leaderboard)
        );
        assert_eq!(features["quotes"], true);
        assert_eq!(features["email_signature"], true);
    }
}

#[test_db::test(seed)]
async fn a_quotes_only_plan_offers_only_quotes(pool: MySqlPool) {
    let server = server(pool);
    for session in [sessions::QUOTES_ONLY_MANAGER, sessions::QUOTES_ONLY_REP] {
        let shell = shell(&server, session).await;
        let features = &shell["company_features"];
        assert_eq!(features["quotes"], true);
        assert_eq!(features["email_signature"], false);
        assert_eq!(features["signature"], false);
        assert_eq!(features["quickbooks"], false);
        assert_eq!(shell["user_signature_access"], false);
        assert_eq!(
            shell["plan"],
            json!({
                "source": "subscription",
                "addons": ["quotes"],
                "manage_url": "https://payments.granite-manager.com/account",
            })
        );
    }
    let features: Value = server
        .get("/v1/company/features")
        .with_session(sessions::QUOTES_ONLY_MANAGER)
        .await
        .json();
    assert_eq!(features["quickbooks_enabled"], false);
    assert_eq!(features["e_signature"], false);
}

#[test_db::test(seed)]
async fn a_comp_signatures_grant_counts_and_an_ended_quotes_item_does_not(pool: MySqlPool) {
    let server = server(pool);
    for session in [
        sessions::COMP_SIGNATURES_MANAGER,
        sessions::COMP_SIGNATURES_REP,
    ] {
        let shell = shell(&server, session).await;
        let features = &shell["company_features"];
        assert_eq!(features["signature"], true);
        assert_eq!(features["email_signature"], true);
        assert_eq!(features["quotes"], false);
        assert_eq!(features["quickbooks"], false);
        // Every user of the company, not only those with the personal flag.
        assert_eq!(shell["user_signature_access"], true);
        assert_eq!(shell["plan"]["addons"], json!(["signatures"]));
        assert_eq!(shell["plan"]["source"], "subscription");
    }
}

#[test_db::test(seed)]
async fn a_super_admin_gets_the_plan_of_the_company_they_act_in(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (103, 9, 20100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let acting: Value = server
        .get("/v1/app/shell")
        .add_header(
            "cookie",
            cookie_for(SessionData {
                session_id: Some(sessions::SUPER_ADMIN.to_string()),
                active_company_id: Some(20100),
            }),
        )
        .await
        .json();
    assert_eq!(acting["company"]["id"], 20100);
    assert_eq!(acting["plan"]["source"], "subscription");
    assert_eq!(acting["plan"]["addons"], json!(["quotes"]));
    assert_eq!(acting["company_features"]["quotes"], true);
    assert_eq!(acting["company_features"]["email_signature"], false);
    let home = shell(&server, sessions::SUPER_ADMIN).await;
    assert_eq!(home["plan"]["source"], "legacy");
}

#[test_db::test(seed)]
async fn superusers_grant_and_take_back_free_addons(pool: MySqlPool) {
    make_superuser(&pool).await;
    let server = server(pool.clone());
    let quotes = |shell: &Value| shell["company_features"]["quotes"].clone();
    assert_eq!(
        quotes(&shell(&server, sessions::COMP_SIGNATURES_MANAGER).await),
        false
    );

    grant(&server, 20101, "quotes", true).await;
    let granted = shell(&server, sessions::COMP_SIGNATURES_MANAGER).await;
    assert_eq!(quotes(&granted), true);
    assert_eq!(granted["plan"]["addons"], json!(["signatures", "quotes"]));
    assert_eq!(
        quotes(&shell(&server, sessions::QUOTES_ONLY_MANAGER).await),
        true
    );
    let details: Value = server
        .get("/v1/superuser/companies/20101")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(details["plan_source"], "subscription");
    assert_eq!(details["comp_addons"], json!(["quotes", "signatures"]));
    let legacy: Value = server
        .get("/v1/superuser/companies/100")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(legacy["plan_source"], "legacy");
    assert_eq!(legacy["comp_addons"], json!([]));

    // Granting again keeps the start; taking back ends it.
    let started: String = sqlx::query_scalar(
        "SELECT CAST(started_at AS CHAR) FROM company_addons
          WHERE company_id = 20101 AND addon_key = 'quotes' AND source = 'comp'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE company_addons SET started_at = '2026-10-02 00:00:00'
          WHERE company_id = 20101 AND addon_key = 'quotes' AND source = 'comp'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_ne!(started, "2026-10-02 00:00:00");
    grant(&server, 20101, "quotes", true).await;
    let kept: String = sqlx::query_scalar(
        "SELECT CAST(started_at AS CHAR) FROM company_addons
          WHERE company_id = 20101 AND addon_key = 'quotes' AND source = 'comp'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, "2026-10-02 00:00:00");

    grant(&server, 20101, "quotes", false).await;
    assert_eq!(
        quotes(&shell(&server, sessions::COMP_SIGNATURES_MANAGER).await),
        false
    );
    assert_eq!(
        quotes(&shell(&server, sessions::QUOTES_ONLY_MANAGER).await),
        true
    );

    grant(&server, 20101, "quotes", true).await;
    assert_eq!(
        quotes(&shell(&server, sessions::COMP_SIGNATURES_MANAGER).await),
        true
    );
    let restarted: String = sqlx::query_scalar(
        "SELECT CAST(started_at AS CHAR) FROM company_addons
          WHERE company_id = 20101 AND addon_key = 'quotes' AND source = 'comp'
            AND ended_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_ne!(restarted, "2026-10-02 00:00:00");
    let granted_by: Option<i32> = sqlx::query_scalar(
        "SELECT granted_by FROM company_addons
          WHERE company_id = 20101 AND addon_key = 'quotes' AND source = 'comp'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(granted_by, Some(102));
}

#[test_db::test(seed)]
async fn taking_back_a_grant_never_ends_a_paid_addon(pool: MySqlPool) {
    make_superuser(&pool).await;
    let server = server(pool.clone());
    grant(&server, 20100, "quotes", false).await;
    let ended: Option<String> = sqlx::query_scalar(
        "SELECT CAST(ended_at AS CHAR) FROM company_addons
          WHERE company_id = 20100 AND addon_key = 'quotes' AND source = 'subscription'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ended, None);
    assert_eq!(
        shell(&server, sessions::QUOTES_ONLY_MANAGER).await["company_features"]["quotes"],
        true
    );
    // A grant on top of a bought add-on is a separate row.
    grant(&server, 20100, "quotes", true).await;
    grant(&server, 20100, "quotes", false).await;
    assert_eq!(
        shell(&server, sessions::QUOTES_ONLY_MANAGER).await["company_features"]["quotes"],
        true
    );
}

#[test_db::test(seed)]
async fn the_addon_route_checks_who_what_and_where(pool: MySqlPool) {
    make_superuser(&pool).await;
    let server = server(pool.clone());
    server
        .put("/v1/superuser/companies/20101/addons/quotes")
        .with_session(sessions::MANAGER)
        .json(&json!({ "enabled": true }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .put("/v1/superuser/companies/20101/addons/quote")
        .with_session(sessions::ADMIN)
        .json(&json!({ "enabled": true }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .put("/v1/superuser/companies/999999/addons/quotes")
        .with_session(sessions::ADMIN)
        .json(&json!({ "enabled": true }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // A legacy company can hold a grant; it has every add-on anyway.
    grant(&server, 100, "signatures", true).await;
    let legacy: Value = server
        .get("/v1/superuser/companies/100")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(legacy["comp_addons"], json!(["signatures"]));
}

#[test_db::test(seed)]
async fn the_walk_in_user_of_a_new_company_is_a_system_user(pool: MySqlPool) {
    make_superuser(&pool).await;
    let server = server(pool.clone());
    let created: Value = server
        .post("/v1/superuser/companies")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Granite New", "subdomain": "granite-new" }))
        .await
        .json();
    let is_system: bool = sqlx::query_scalar(
        "SELECT is_system <> 0 FROM users WHERE company_id = ? AND name = 'Walk-in'",
    )
    .bind(created["id"].as_i64().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(is_system);
}
