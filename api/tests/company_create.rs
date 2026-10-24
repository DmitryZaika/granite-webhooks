mod common;

use api::company_create::{
    CompanyInit, OwnerInit, PlanSource, create_company_tx, create_owner_tx, valid_subdomain,
};
use api::state::AppState;
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET};
use serde_json::{Value, json};
use sqlx::MySqlPool;

fn init(subdomain: Option<&str>, plan_source: PlanSource) -> CompanyInit {
    CompanyInit {
        name: "Self Serve Stone".into(),
        subdomain: subdomain.map(str::to_string),
        address: None,
        state_taxes: None,
        domain: None,
        cloudtalk_access_key: None,
        cloudtalk_access_secret: None,
        ringcentral_client_id: None,
        ringcentral_client_secret: None,
        ringcentral_jwt: None,
        ringcentral_server_url: None,
        plan_source,
    }
}

#[test]
fn reserved_and_malformed_subdomains_are_invalid() {
    assert!(valid_subdomain("granite-new"));
    assert!(!valid_subdomain("payments"));
    assert!(!valid_subdomain("www"));
    assert!(!valid_subdomain("Bad Sub"));
    assert!(!valid_subdomain("-a"));
    assert!(!valid_subdomain(""));
}

#[test_db::test(seed)]
async fn a_self_serve_company_has_no_subdomain_and_requires_billing(pool: MySqlPool) {
    let mut tx = pool.begin().await.unwrap();
    let id = create_company_tx(&mut tx, &init(None, PlanSource::Subscription))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (subdomain, plan, required, at_set): (Option<String>, String, i32, i32) = sqlx::query_as(
        "SELECT subdomain, plan_source, billing_required, billing_required_at IS NOT NULL
         FROM company WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (subdomain, plan.as_str(), required, at_set),
        (None, "subscription", 1, 1)
    );
    let max_users: Option<i32> = sqlx::query_scalar("SELECT max_users FROM company WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(max_users, None, "per-user pricing: no user limit");
    let (email, is_system, password): (String, i32, String) = sqlx::query_as(
        "SELECT email, is_system, password FROM users WHERE company_id = ? AND name = 'Walk-in'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(email, format!("walk-in-{id}@granite-manager.invalid"));
    assert_eq!(is_system, 1);
    assert!(password.starts_with('!') && password.len() == 65);
}

#[test_db::test(seed)]
async fn a_legacy_company_keeps_the_column_defaults(pool: MySqlPool) {
    let mut conn = pool.acquire().await.unwrap();
    let id = create_company_tx(&mut conn, &init(Some("legacy-co"), PlanSource::Legacy))
        .await
        .unwrap();
    let (plan, required, max_users): (String, i32, Option<i32>) =
        sqlx::query_as("SELECT plan_source, billing_required, max_users FROM company WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (plan.as_str(), required, max_users),
        ("legacy", 0, Some(10))
    );
    let email: String =
        sqlx::query_scalar("SELECT email FROM users WHERE company_id = ? AND name = 'Walk-in'")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(email, "walk-in@legacy-co.com");
}

#[test_db::test(seed)]
async fn the_owner_is_manager_and_admin_and_can_sign_in(pool: MySqlPool) {
    let hash = bcrypt::hash("owner-secret-1", 10).unwrap();
    let mut tx = pool.begin().await.unwrap();
    let company = create_company_tx(&mut tx, &init(None, PlanSource::Subscription))
        .await
        .unwrap();
    let owner = create_owner_tx(
        &mut tx,
        company,
        &OwnerInit {
            name: "Olga Owner",
            email: "olga@selfserve.test",
            password_hash: &hash,
            phone: Some("5551230000"),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (admin, employee, system, company_id): (i32, i32, i32, i32) = sqlx::query_as(
        "SELECT is_admin, is_employee, is_system, company_id FROM users WHERE id = ?",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((admin, employee, system, company_id), (1, 1, 0, company));
    let positions: Vec<(i32, i32)> =
        sqlx::query_as("SELECT position_id, company_id FROM users_positions WHERE user_id = ?")
            .bind(owner)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(positions, vec![(5, company)]);

    let server = TestServer::new(api::app(AppState::new(
        pool.clone(),
        SECRET,
        ALLOWED_ORIGIN,
    )));
    let response = server
        .post("/v1/auth/login")
        .json(&json!({ "email": "olga@selfserve.test", "password": "owner-secret-1" }))
        .await;
    response.assert_status_ok();
    assert!(
        response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| v.starts_with("__session=") && !v.starts_with("__session=;"))
    );
    let body: Value = response.json();
    assert_eq!(i64::from(owner), body["user"]["id"].as_i64().unwrap());
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE user_id = ?")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 1);
}
