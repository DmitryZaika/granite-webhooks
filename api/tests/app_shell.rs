mod common;

use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn the_shell_describes_the_signed_in_frame(pool: MySqlPool) {
    sqlx::raw_sql(
        "UPDATE company SET logo_url = 'https://img.example/acme.png', logo_height = 64,
                cloudtalk_access_key = 'k', cloudtalk_access_secret = 's' WHERE id = 100;
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (100, 3, 100), (101, 5, 100);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    server
        .get("/v1/app/shell")
        .await
        .assert_status_unauthorized();
    let rep: Value = server
        .get("/v1/app/shell")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(rep["user"]["id"], 100);
    assert_eq!(
        rep["company"],
        json!({ "id": 100, "name": "Acme Stone", "logo_url": "https://img.example/acme.png", "logo_height": 64 })
    );
    assert_eq!(rep["is_shop_manager"], true);
    assert_eq!(rep["can_manage_company"], false);
    assert_eq!(rep["has_cloudtalk_api"], true);
    assert_eq!(rep["portal"], Value::Null);
    assert_eq!(rep["user_is_super_admin"], false);
    let manager: Value = server
        .get("/v1/app/shell")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(manager["can_manage_company"], true);
    assert_eq!(manager["can_manage_billing"], true);
    let super_admin: Value = server
        .get("/v1/app/shell")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(super_admin["user_is_super_admin"], true);
    assert_eq!(
        super_admin["superadmin_companies"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test_db::test(seed)]
async fn installers_are_locked_into_their_portal(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 6, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let shell: Value = server
        .get("/v1/app/shell")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(shell["portal"], "installer");
    assert_eq!(shell["portal_home"], "/installers/100/checklist");
}

#[test_db::test(seed)]
async fn template_makers_are_locked_into_their_schedule(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 11, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let shell: Value = server
        .get("/v1/app/shell")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(shell["portal"], "template");
    assert_eq!(shell["portal_home"], "/templates/100/schedule");
}

#[test_db::test(seed)]
async fn supplier_lists_can_be_limited_to_used_ones(pool: MySqlPool) {
    let server = server(pool);
    let all: Value = server
        .get("/v1/suppliers")
        .with_session(sessions::REP)
        .await
        .json();
    let with_stones: Value = server
        .get("/v1/suppliers?has=stones")
        .with_session(sessions::REP)
        .await
        .json();
    assert!(with_stones.as_array().unwrap().len() <= all.as_array().unwrap().len());
    server
        .get("/v1/suppliers?has=boats")
        .with_session(sessions::REP)
        .await
        .assert_status_bad_request();
}

#[test_db::test(seed)]
async fn the_shell_says_what_the_home_company_can_do(pool: MySqlPool) {
    let server = common::server(pool);
    let shell = server
        .get("/v1/app/shell")
        .with_session(common::sessions::REP)
        .await
        .json::<serde_json::Value>();
    let capabilities = &shell["feature_capabilities"];
    for key in [
        "cloudtalk",
        "quickbooks",
        "verified_domain",
        "sms_provider",
        "followup_tours",
        "onboarding",
    ] {
        assert!(capabilities[key].is_boolean(), "{key}");
    }
}

#[test_db::test(seed)]
async fn a_company_frame_needs_no_session(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO stone_types (company_id, name) VALUES (100, 'Quartz'), (100, 'Granite');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = common::server(pool);
    let frame = server
        .get("/v1/public/companies/100/frame")
        .await
        .json::<serde_json::Value>();
    assert_eq!(frame["name"], "Acme Stone", "{frame}");
    assert!(
        frame["stone_types"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "Quartz")
    );
    assert!(frame["stone_suppliers"].is_array());
    let unknown = server
        .get("/v1/public/companies/999999/frame")
        .await
        .json::<serde_json::Value>();
    assert_eq!(unknown["name"], serde_json::Value::Null);
}
