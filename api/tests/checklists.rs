mod common;

use api::auth::cookie::SessionData;
use axum::http::StatusCode;
use common::{WithSession, cookie_for, ids, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: checklists 9000-9006 (9005 has no company, 9006 is company 101's).

async fn list(pool: MySqlPool, session: &str) -> Value {
    let res = server(pool)
        .get("/v1/checklists")
        .with_session(session)
        .await;
    res.assert_status_ok();
    res.json()
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn lists_company_checklists_newest_first(pool: MySqlPool) {
    let rows = list(pool, sessions::REP).await;
    // 9003 and 9001 share a timestamp: the higher id first. 9004 is named
    // like the onboarding sample; hiding it is the page's business.
    assert_eq!(ids(&rows), vec![9003, 9001, 9002, 9000, 9004]);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn checklist_has_every_field(pool: MySqlPool) {
    let rows = list(pool, sessions::REP).await;
    assert_eq!(
        rows[3],
        json!({
            "id": 9000,
            "customer_id": 1000,
            "customer_name": "Alice Lead",
            "installation_address": "1 Main St, Indianapolis, IN",
            "email": "alice@example.test",
            "installer_id": 100,
            "installer_name": "Rita Rep",
            "checks": {
                "material_correct": true,
                "seams_satisfaction": true,
                "appliances_fit": true,
                "backsplashes_correct": true,
                "edges_correct": true,
                "holes_drilled": true,
                "cleanup_completed": true
            },
            "comments": "Great job, very clean.",
            "created_at": "2026-05-10T14:00:00.000Z"
        })
    );
    // No customer, installer, email or comment; mixed checks.
    assert_eq!(
        rows[1],
        json!({
            "id": 9001,
            "customer_id": null,
            "customer_name": "Bob Walk-in",
            "installation_address": "22 Side Rd, Carmel, IN",
            "email": null,
            "installer_id": null,
            "installer_name": null,
            "checks": {
                "material_correct": true,
                "seams_satisfaction": false,
                "appliances_fit": true,
                "backsplashes_correct": false,
                "edges_correct": true,
                "holes_drilled": false,
                "cleanup_completed": false
            },
            "comments": null,
            "created_at": "2026-05-12T02:00:00.000Z"
        })
    );
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn deleted_installers_keep_their_name(pool: MySqlPool) {
    let rows = list(pool, sessions::REP).await;
    assert_eq!(rows[2]["id"], 9002);
    assert_eq!(rows[2]["installer_id"], 105);
    assert_eq!(rows[2]["installer_name"], "Gus Gone");
    assert_eq!(rows[2]["comments"], "");
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn never_returns_the_signature(pool: MySqlPool) {
    let rows = list(pool, sessions::REP).await;
    for row in rows.as_array().unwrap() {
        assert!(row.get("signature").is_none(), "{row}");
    }
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn tenants_are_isolated(pool: MySqlPool) {
    let rows = list(pool.clone(), sessions::OTHER_COMPANY_REP).await;
    assert_eq!(ids(&rows), vec![9006]);
    assert_eq!(rows[0]["installer_name"], "Olga Other");
    // A checklist without a company belongs to nobody.
    let rows = list(pool, sessions::ADMIN).await;
    assert!(!ids(&rows).contains(&9005));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn super_admin_sees_the_company_they_switched_to(pool: MySqlPool) {
    // Sam Super belongs to company 101 and is super admin in 100.
    let rows = list(pool.clone(), sessions::SUPER_ADMIN).await;
    assert_eq!(ids(&rows), vec![9006]);
    let switched = cookie_for(SessionData {
        session_id: Some(sessions::SUPER_ADMIN.into()),
        active_company_id: Some(100),
    });
    let rows: Value = server(pool)
        .get("/v1/checklists")
        .add_header("cookie", switched)
        .await
        .json();
    assert_eq!(ids(&rows), vec![9003, 9001, 9002, 9000, 9004]);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn requires_an_employee_or_admin(pool: MySqlPool) {
    let server = server(pool);
    server
        .get("/v1/checklists")
        .await
        .assert_status_unauthorized();
    server
        .get("/v1/checklists")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/checklists")
        .with_session(sessions::DELETED_USER)
        .await
        .assert_status_unauthorized();
    server
        .get("/v1/checklists")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
}
