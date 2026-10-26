use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Rita (user 100) joins company 100's marketing team.
async fn make_marketer(pool: &MySqlPool) {
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (100, 7, 100)",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn only_the_company_marketing_team_gets_in(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .get("/v1/marketing/100/leads")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    make_marketer(&pool).await;
    server
        .get("/v1/marketing/100/leads")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
    server
        .get("/v1/marketing/101/leads")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/marketing/100/leads")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn a_superuser_viewing_as_marketing_gets_in(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .get("/v1/marketing/100/leads")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(&pool)
        .await
        .unwrap();
    server
        .get("/v1/marketing/100/leads")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn referral_sources_are_managed_per_company(pool: MySqlPool) {
    make_marketer(&pool).await;
    let server = server(pool);
    let res = server
        .post("/v1/marketing/100/referral-sources")
        .with_session(sessions::REP)
        .json(&json!({ "name": " Facebook " }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    server
        .post("/v1/marketing/100/referral-sources")
        .with_session(sessions::REP)
        .json(&json!({ "name": "facebook" }))
        .await
        .assert_status(StatusCode::CONFLICT);
    server
        .patch(&format!("/v1/marketing/100/referral-sources/{id}"))
        .with_session(sessions::REP)
        .json(&json!({ "name": "Meta" }))
        .await
        .assert_status_ok();
    let names: Value = server
        .get("/v1/marketing/100/referral-sources")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(names, json!([{ "id": id, "name": "Meta" }]));
    server
        .delete(&format!("/v1/marketing/100/referral-sources/{id}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let names: Value = server
        .get("/v1/marketing/100/referral-sources")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(names, json!([]));
}

#[test_db::test(seed)]
async fn a_lead_is_added_edited_listed_and_deleted(pool: MySqlPool) {
    make_marketer(&pool).await;
    sqlx::query(
        "INSERT INTO marketing_refferal_sources (company_id, name) VALUES (100, 'facebook')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());
    let lead = json!({
        "name": "Mia Marketing", "emails": ["mia@example.test", "mia2@example.test"],
        "phone": "317-555-0199", "referral_source": "Facebook", "your_message": "Quartz please",
    });
    let mut unknown = lead.clone();
    unknown["referral_source"] = json!("tv");
    server
        .post("/v1/marketing/100/leads")
        .with_session(sessions::REP)
        .json(&unknown)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let res = server
        .post("/v1/marketing/100/leads")
        .with_session(sessions::REP)
        .json(&lead)
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    let (source, created_by): (String, String) =
        sqlx::query_as("SELECT source, created_by FROM customers WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (source.as_str(), created_by.as_str()),
        ("leads", "Marketing team")
    );

    let page: Value = server
        .get("/v1/marketing/100/leads?channel=facebook")
        .with_session(sessions::REP)
        .await
        .json();
    let leads = page["leads"].as_array().unwrap();
    assert_eq!(leads[0]["id"], id);
    assert_eq!(leads[0]["email"], "mia@example.test");
    assert!(page["stages"].as_array().unwrap().len() > 1);

    let form: Value = server
        .get(&format!("/v1/marketing/100/leads/{id}"))
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        form["emails"],
        json!(["mia@example.test", "mia2@example.test"])
    );
    server
        .put(&format!("/v1/marketing/100/leads/{id}"))
        .with_session(sessions::REP)
        .json(
            &json!({ "name": "Mia M.", "emails": ["mia2@example.test"], "phone": "317-555-0197" }),
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let form: Value = server
        .get(&format!("/v1/marketing/100/leads/{id}"))
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(form["name"], "Mia M.");
    assert_eq!(form["emails"], json!(["mia2@example.test"]));

    // Marked invalid: counted in the stats.
    sqlx::query("UPDATE customers SET invalid_lead = 'Out of area' WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let page: Value = server
        .get("/v1/marketing/100/leads")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(page["invalid_stats"]["facebook"], 1);
    assert_eq!(page["invalid_reasons"]["out_of_area"], 1);

    let card: Value = server
        .get(&format!("/v1/marketing/100/leads/{id}/info"))
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(card["customer"]["name"], "Mia M.");
    assert_eq!(card["history"]["emails"], json!([]));
    let projects: Value = server
        .get(&format!("/v1/marketing/100/leads/{id}/projects"))
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(projects[0]["id"], id);

    server
        .delete(&format!("/v1/marketing/100/leads/{id}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get(&format!("/v1/marketing/100/leads/{id}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete(&format!("/v1/marketing/100/leads/{id}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn the_marketing_team_searches_customers(pool: MySqlPool) {
    make_marketer(&pool).await;
    let server = server(pool);
    let rows: Value = server
        .get("/v1/marketing/100/customers/search?term=Alice&by=name")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(rows[0]["name"], "Alice Lead");
    server
        .get("/v1/marketing/101/customers/search?term=Alice")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}
