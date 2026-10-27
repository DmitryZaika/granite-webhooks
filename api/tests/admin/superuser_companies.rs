use crate::common;

use api::app;
use api::deliverability::{AppDns, FixedDns};
use api::state::{AppState, SesIdentities};
use axum::http::StatusCode;
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// Ada Admin (102) is made a superuser; company 100 is Acme Stone (subdomain
// acme), 101 Other Granite Co.

fn dns() -> FixedDns {
    FixedDns {
        mx: HashMap::from([
            (
                "healthy.example".to_string(),
                vec![("aspmx.l.google.com".to_string(), 1)],
            ),
            (
                "acme.example".to_string(),
                vec![("inbound-smtp.us-east-2.amazonaws.com".to_string(), 10)],
            ),
        ]),
        txt: HashMap::from([
            (
                "healthy.example".to_string(),
                vec!["v=spf1 include:_spf.google.com -all".to_string()],
            ),
            (
                "_dmarc.healthy.example".to_string(),
                vec!["v=DMARC1; p=reject".to_string()],
            ),
            (
                "google._domainkey.healthy.example".to_string(),
                vec!["v=DKIM1; p=abc".to_string()],
            ),
        ]),
        a: HashMap::from([(
            "acme.granite-manager.com".to_string(),
            vec!["10.0.0.1".parse().unwrap()],
        )]),
    }
}

async fn server(pool: &MySqlPool) -> TestServer {
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(pool)
        .await
        .unwrap();
    let verified: HashSet<String> = ["acme.example".to_string()].into();
    let pending: HashSet<String> = ["pending.example".to_string()].into();
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_app_url("https://granite-manager.com")
        .with_dns(AppDns::Fixed(Arc::new(dns())))
        .with_ses(SesIdentities::Fixed(Some(Arc::new((verified, pending)))));
    TestServer::new(app(state))
}

#[test_db::test(seed)]
async fn superusers_list_the_companies(pool: MySqlPool) {
    sqlx::query("UPDATE company SET cloudtalk_access_key = 'k', cloudtalk_access_secret = 's', qbo_realm_id = '5', billing_required = 1, max_users = 20 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(&pool).await;
    server
        .get("/v1/superuser/companies")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let rows: Value = server
        .get("/v1/superuser/companies")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let acme = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == 100)
        .unwrap();
    assert_eq!(acme["name"], "Acme Stone");
    assert_eq!(acme["subdomain"], "acme");
    assert_eq!(acme["max_users"], 20);
    assert_eq!(acme["cloudtalk_connected"], true);
    assert_eq!(acme["quickbooks_connected"], true);
    assert_eq!(acme["billing_required"], true);
    assert_eq!(acme["features"]["quickbooks"], true);
    assert!(acme["user_count"].as_i64().unwrap() >= 4);
    assert!(
        acme["onboarding_eligible"].as_i64().unwrap()
            >= acme["onboarding_finished"].as_i64().unwrap()
    );
    // Most users first.
    let counts: Vec<i64> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["user_count"].as_i64().unwrap())
        .collect();
    let mut sorted = counts.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(counts, sorted);
}

#[test_db::test(seed)]
async fn creating_a_company_seeds_it(pool: MySqlPool) {
    let server = server(&pool).await;
    for body in [
        json!({ "name": " ", "subdomain": "new" }),
        json!({ "name": "New", "subdomain": "Bad Sub" }),
        json!({ "name": "New", "subdomain": "new", "sales_tax_percent": 120 }),
    ] {
        server
            .post("/v1/superuser/companies")
            .with_session(sessions::ADMIN)
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    server
        .post("/v1/superuser/companies")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Dup", "subdomain": "acme" }))
        .await
        .assert_status(StatusCode::CONFLICT);
    let created: Value = server
        .post("/v1/superuser/companies")
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "name": "Granite New", "subdomain": "granite-new", "sales_tax_percent": 7.25,
                       "domain": "owner@gmail.com", "address": "" }),
        )
        .await
        .json();
    let id = created["id"].as_i64().unwrap();
    let (tax, hidden, address): (String, i32, Option<String>) = sqlx::query_as(
        "SELECT CAST(state_taxes AS CHAR), statistics_widgets_hidden, address FROM company WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((tax.as_str(), hidden, address), ("0.0725", 1, None));
    let count = |sql: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        count("SELECT COUNT(*) FROM calendars WHERE company_id = ?").await,
        3
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM email_templates WHERE company_id = ? AND user_id IS NULL")
            .await,
        4
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM customer_sources WHERE company_id = ? AND is_system = 1").await,
        4
    );
    let walk_in: (String, String) =
        sqlx::query_as("SELECT name, email FROM users WHERE company_id = ? AND is_employee = 1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        walk_in,
        ("Walk-in".into(), "walk-in@granite-new.com".into())
    );
    // A free mail domain is high risk without asking DNS.
    let trust: Value = server
        .get(&format!("/v1/superuser/companies/{id}/domain-trust"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(trust["domain"], "gmail.com");
    assert_eq!(trust["risk_level"], "HIGH_RISK");
    assert_eq!(trust["reputation_score"], 85);
}

#[test_db::test(seed)]
async fn superusers_edit_and_delete_a_company(pool: MySqlPool) {
    let server = server(&pool).await;
    let company: Value = server
        .get("/v1/superuser/companies/101")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(company["name"], "Other Granite Co");
    assert_eq!(company["quickbooks_enabled"], true);
    assert_eq!(company["signature_enabled"], false);
    server
        .put("/v1/superuser/companies/101")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Other", "subdomain": "acme" }))
        .await
        .assert_status(StatusCode::CONFLICT);
    server
        .put("/v1/superuser/companies/101")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Other Co", "subdomain": "other", "max_users": 5, "sales_tax_percent": 6,
                       "signwell_api_key": "sw-key", "signature_enabled": true, "quickbooks_enabled": false,
                       "domain": "healthy.example" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let company: Value = server
        .get("/v1/superuser/companies/101")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(company["subdomain"], "other");
    assert_eq!(company["max_users"], 5);
    assert_eq!(company["sales_tax_percent"], 6.0);
    assert_eq!(company["signwell_api_key"], "sw-key");
    assert_eq!(company["signature_enabled"], true);
    assert_eq!(company["quickbooks_enabled"], false);
    assert_eq!(company["logo_height"], 48);
    // The new domain was checked.
    let trust: Option<String> =
        sqlx::query_scalar("SELECT risk_level FROM company_domain_trust WHERE company_id = 101")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(trust.as_deref(), Some("GOOD"));

    server
        .delete("/v1/superuser/companies/101?confirm_name=Wrong")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .delete("/v1/superuser/companies/101?confirm_name=Other%20Co")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get("/v1/superuser/companies/101")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn company_users_show_roles_and_tours(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT IGNORE INTO positions (id, name) VALUES (8, 'check-in');
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 5, 100), (104, 8, 100);
         UPDATE users SET cloudtalk_agent_id = '77' WHERE id = 100;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(&pool).await;
    let body: Value = server
        .get("/v1/superuser/companies/100/users")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let user = |id: i64| {
        body["users"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(user(101)["can_manage_users"], true);
    assert_eq!(user(100)["cloudtalk_agent_id"], "77");
    assert_eq!(user(104)["runs_onboarding"], false);
    assert_eq!(user(104)["counts_toward_user_limit"], false);
    assert_eq!(user(102)["is_superuser"], true);
    assert_eq!(user(100)["progress"]["completed_module_ids"], json!([]));
    assert_eq!(body["deleted_count"], 1);
    assert_eq!(body["has_cloudtalk_api"], false);
}

#[test_db::test(seed)]
async fn domain_trust_is_scored_from_dns_and_reused(pool: MySqlPool) {
    sqlx::raw_sql("UPDATE company SET domain = 'healthy.example' WHERE id = 100; UPDATE company SET domain = NULL WHERE id = 101;")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(&pool).await;
    let trust: Value = server
        .get("/v1/superuser/companies/100/domain-trust")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(trust["domain"], "healthy.example");
    assert_eq!(trust["reputation_score"], 0);
    assert_eq!(trust["risk_level"], "GOOD");
    assert!(
        trust["reputation_reasons"][0]
            .as_str()
            .unwrap()
            .starts_with("Inbox risk check v5 for healthy.example")
    );
    assert!(
        trust["reputation_reasons"][1]
            .as_str()
            .unwrap()
            .starts_with("Confidence: high")
    );
    // A week-old check of the same domain is reused.
    sqlx::query("UPDATE company_domain_trust SET reputation_score = 3 WHERE company_id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let again: Value = server
        .get("/v1/superuser/companies/100/domain-trust")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(again["reputation_score"], 3);
    // No domain: null.
    let none: Value = server
        .get("/v1/superuser/companies/101/domain-trust")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(none, Value::Null);
}

#[test_db::test(seed)]
async fn ses_inbound_needs_the_identity_and_the_mx(pool: MySqlPool) {
    sqlx::raw_sql(
        "UPDATE company SET domain = 'acme.example' WHERE id = 100;
         UPDATE company SET domain = 'pending.example' WHERE id = 101;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(&pool).await;
    let acme: Value = server
        .get("/v1/superuser/companies/100/ses-inbound")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(
        acme,
        json!({ "domain": "acme.example", "ses_verified": true, "ses_pending": false,
                "mx_inbound": true, "inbound_ready": true })
    );
    let other: Value = server
        .get("/v1/superuser/companies/101/ses-inbound")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(
        other,
        json!({ "domain": "pending.example", "ses_verified": false, "ses_pending": true,
                "mx_inbound": false, "inbound_ready": false })
    );
}

#[test_db::test(seed)]
async fn the_setup_checklist_checks_and_takes_ticks(pool: MySqlPool) {
    sqlx::raw_sql(
        "UPDATE company SET domain = 'acme.example' WHERE id = 100;
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 5, 100);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(&pool).await;
    let checklist: Value = server
        .get("/v1/superuser/companies/100/setup-checklist")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let item = |list: &Value, key: &str| {
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["key"] == key)
            .unwrap()
            .clone()
    };
    assert_eq!(
        item(&checklist, "subdomain_infra"),
        json!({ "key": "subdomain_infra", "label": "Add subdomain to Caddy and AWS", "done": true,
                "auto": true, "manual": false, "source": "auto",
                "detail": "Expect DNS for acme.granite-manager.com" })
    );
    assert_eq!(item(&checklist, "domain")["detail"], "acme.example");
    assert_eq!(item(&checklist, "users.manager")["done"], true);
    assert_eq!(item(&checklist, "users.installer")["done"], false);
    assert_eq!(item(&checklist, "inbound_email")["done"], true);
    assert_eq!(item(&checklist, "onboarding.manager")["done"], false);
    assert_eq!(checklist["total_count"], 15);

    server
        .put("/v1/superuser/companies/100/setup-checklist/items/onboarding.manager")
        .with_session(sessions::ADMIN)
        .json(&json!({ "checked": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .put("/v1/superuser/companies/100/setup-checklist/items/nope")
        .with_session(sessions::ADMIN)
        .json(&json!({ "checked": true }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let checklist: Value = server
        .get("/v1/superuser/companies/100/setup-checklist")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(item(&checklist, "onboarding.manager")["source"], "manual");
    server
        .put("/v1/superuser/companies/100/setup-checklist/items/onboarding.manager")
        .with_session(sessions::ADMIN)
        .json(&json!({ "checked": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let checklist: Value = server
        .get("/v1/superuser/companies/100/setup-checklist")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(item(&checklist, "onboarding.manager")["done"], false);
    // QuickBooks off: no QuickBooks item.
    sqlx::query("UPDATE company SET quickbooks_enabled = 0 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let checklist: Value = server
        .get("/v1/superuser/companies/100/setup-checklist")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(checklist["total_count"], 14);
}

#[test_db::test(seed)]
async fn superusers_switch_features_and_billing(pool: MySqlPool) {
    let server = server(&pool).await;
    for (feature, enabled) in [
        ("quickbooks", false),
        ("signature", true),
        ("onboarding", false),
        ("leaderboard", true),
        ("projects", true),
    ] {
        server
            .put(&format!("/v1/superuser/companies/100/features/{feature}"))
            .with_session(sessions::ADMIN)
            .json(&json!({ "enabled": enabled }))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    server
        .put("/v1/superuser/companies/100/features/sql")
        .with_session(sessions::ADMIN)
        .json(&json!({ "enabled": true }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .put("/v1/superuser/companies/99999/features/quickbooks")
        .with_session(sessions::ADMIN)
        .json(&json!({ "enabled": true }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put("/v1/superuser/companies/100/billing-required")
        .with_session(sessions::ADMIN)
        .json(&json!({ "enabled": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let rows: Value = server
        .get("/v1/superuser/companies")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let acme = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == 100)
        .unwrap();
    assert_eq!(
        acme["features"],
        json!({ "quickbooks": false, "signature": true, "onboarding": false, "leaderboard": true, "projects": true })
    );
    assert_eq!(acme["billing_required"], true);
    server
        .put("/v1/superuser/companies/100/billing-required")
        .with_session(sessions::REP)
        .json(&json!({ "enabled": false }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn every_seeded_company_reads(pool: MySqlPool) {
    let server = server(&pool).await;
    for id in [100, 101] {
        let res = server
            .get(&format!("/v1/superuser/companies/{id}"))
            .with_session(sessions::ADMIN)
            .await;
        assert_eq!(res.status_code(), StatusCode::OK, "{}", res.text());
    }
}
