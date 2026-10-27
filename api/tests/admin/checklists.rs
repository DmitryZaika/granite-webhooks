use crate::common;

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

#[test_db::test(seed)]
async fn lists_company_checklists_newest_first(pool: MySqlPool) {
    let rows = list(pool, sessions::REP).await;
    // 9003 and 9001 share a timestamp: the higher id first. 9004 is named
    // like the onboarding sample; hiding it is the page's business.
    assert_eq!(ids(&rows), vec![9003, 9001, 9002, 9000, 9004]);
}

#[test_db::test(seed)]
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

#[test_db::test(seed)]
async fn deleted_installers_keep_their_name(pool: MySqlPool) {
    let rows = list(pool, sessions::REP).await;
    assert_eq!(rows[2]["id"], 9002);
    assert_eq!(rows[2]["installer_id"], 105);
    assert_eq!(rows[2]["installer_name"], "Gus Gone");
    assert_eq!(rows[2]["comments"], "");
}

#[test_db::test(seed)]
async fn never_returns_the_signature(pool: MySqlPool) {
    let rows = list(pool, sessions::REP).await;
    for row in rows.as_array().unwrap() {
        assert!(row.get("signature").is_none(), "{row}");
    }
}

#[test_db::test(seed)]
async fn tenants_are_isolated(pool: MySqlPool) {
    let rows = list(pool.clone(), sessions::OTHER_COMPANY_REP).await;
    assert_eq!(ids(&rows), vec![9006]);
    assert_eq!(rows[0]["installer_name"], "Olga Other");
    // A checklist without a company belongs to nobody.
    let rows = list(pool, sessions::ADMIN).await;
    assert!(!ids(&rows).contains(&9005));
}

#[test_db::test(seed)]
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

#[test_db::test(seed)]
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

#[test_db::test(seed)]
async fn an_installer_saves_a_checklist(pool: MySqlPool) {
    let server = server(pool.clone());
    let body = serde_json::json!({
        "customer_name": "Alice Lead", "installation_address": "1 Main St",
        "material_correct": "on", "edges_correct": "", "signature": "data:image/png;base64,AAAA",
        "email": "alice@example.com", "customer_id": 1000,
    });
    server
        .post("/v1/checklists")
        .with_session(sessions::REP)
        .json(&body)
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN);
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (100, 6, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let res = server
        .post("/v1/checklists")
        .with_session(sessions::REP)
        .json(&body)
        .await;
    res.assert_status(axum::http::StatusCode::CREATED);
    let id = res.json::<serde_json::Value>()["id"].as_u64().unwrap();
    let saved: (i32, bool, bool, i32) = sqlx::query_as(
        "SELECT installer_id, material_correct <> 0, edges_correct <> 0, company_id FROM checklists WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(saved, (100, true, false, 100));
    let mut unsigned = body.clone();
    unsigned["signature"] = "".into();
    server
        .post("/v1/checklists")
        .with_session(sessions::REP)
        .json(&unsigned)
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn a_checklist_prints_as_a_pdf(pool: MySqlPool) {
    sqlx::query("UPDATE checklists SET comments = 'Great job, very clean. The seams are invisible and the edges match the sample exactly, thank you all.' WHERE id = 9000")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(pool);
    let res = server
        .get("/v1/checklists/9000/pdf")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    assert_eq!(res.header("content-type"), "application/pdf");
    assert_eq!(
        res.header("content-disposition"),
        "inline; filename=\"checklist-Alice_Lead-9000.pdf\""
    );
    let bytes = res.as_bytes();
    assert!(bytes.starts_with(b"%PDF-"));
    if let Ok(path) = std::env::var("CHECKLIST_PDF_OUT") {
        std::fs::write(path, bytes.as_ref()).unwrap();
    }
    server
        .get("/v1/checklists/9000/pdf")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/checklists/990000051/pdf?samples=true")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn the_installer_portal_gets_its_company_and_logo(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .get("/v1/installer/context")
        .with_session(sessions::REP)
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN);
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (100, 6, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE company SET logo_url = 'https://img.example/logo.png' WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let body: serde_json::Value = server
        .get("/v1/installer/context")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        body,
        serde_json::json!({
            "company_id": 100,
            "logo_url": "https://img.example/logo.png",
            "quickbooks": false,
        })
    );
}
