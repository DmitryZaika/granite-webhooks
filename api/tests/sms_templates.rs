mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: company template 8671 "Quoted sms" drips 1 hour after a deal enters
// list 101. Company 100 gets CloudTalk here (texting needs a provider).

async fn with_cloudtalk(pool: &MySqlPool) {
    sqlx::raw_sql(
        "UPDATE company SET cloudtalk_access_key = 'k', cloudtalk_access_secret = 's' WHERE id = 100;
         INSERT INTO cloudtalk_sms_templates (id, company_id, user_id, name, body, position)
           VALUES (8672, 100, 100, 'Rita own', 'Hi {{name}}', 0),
                  (8673, 100, 101, 'Mark own', 'Hello', 0);",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn names(list: &Value) -> Vec<&str> {
    list["templates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn reps_see_company_and_own_templates(pool: MySqlPool) {
    server(pool.clone())
        .get("/v1/sms-templates")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    with_cloudtalk(&pool).await;
    let res = server(pool.clone())
        .get("/v1/sms-templates")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    let list: Value = res.json();
    assert_eq!(names(&list), ["Quoted sms", "Rita own"]);
    assert_eq!(list["templates"][0]["scope"], "company");
    assert_eq!(list["templates"][0]["can_edit"], false);
    assert_eq!(list["templates"][0]["lead_list_id"], 101);
    assert_eq!(list["templates"][1]["can_edit"], true);
    assert_eq!(list["deal_lists"], json!([]));

    server(pool.clone())
        .get("/v1/sms-templates?scope=company")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let company: Value = server(pool)
        .get("/v1/sms-templates?scope=company")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(names(&company), ["Quoted sms"]);
    assert_eq!(company["templates"][0]["can_edit"], true);
    assert!(
        company["deal_lists"]
            .as_array()
            .unwrap()
            .iter()
            .any(|list| list["id"] == 101 && list["group_name"] == "Sales")
    );
}

#[test_db::test(seed)]
async fn templates_are_created_with_checks(pool: MySqlPool) {
    with_cloudtalk(&pool).await;
    let server = server(pool.clone());
    let res = server
        .post("/v1/sms-templates")
        .with_session(sessions::REP)
        .json(&json!({ "name": " Follow up ", "body": "Hi {{name}}" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let created: Value = res.json();
    assert_eq!(created["name"], "Follow up");
    assert_eq!(created["scope"], "personal");
    assert_eq!(created["position"], 1);

    for body in [
        json!({ "name": "x", "body": "Hi {{name" }),
        json!({ "name": "", "body": "Hi" }),
        json!({ "name": "x", "body": "a".repeat(1601) }),
    ] {
        server
            .post("/v1/sms-templates")
            .with_session(sessions::REP)
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    server
        .post("/v1/sms-templates")
        .with_session(sessions::REP)
        .json(&json!({ "name": "x", "body": "y", "scope": "company" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);

    let taken = server
        .post("/v1/sms-templates")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Drip", "body": "y", "scope": "company", "drip": { "lead_list_id": 101, "hour_delay": 1 } }))
        .await;
    taken.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        taken.json::<Value>()["error"],
        "Another SMS template already uses this list and delay."
    );
    let drip: Value = server
        .post("/v1/sms-templates")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Drip", "body": "y", "scope": "company", "drip": { "lead_list_id": 101, "hour_delay": 24 } }))
        .await
        .json();
    assert_eq!(drip["scope"], "company");
    assert_eq!(drip["hour_delay"], 24);
    assert_eq!(drip["minute_delay"], 0);
    assert_eq!(drip["customer_sources"], json!(["leads"]));

    // Same hour, other minutes: a separate step; sources are normalized.
    let res = server
        .post("/v1/sms-templates")
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "name": "Drip 30", "body": "y", "scope": "company", "drip": {
            "lead_list_id": 101, "hour_delay": 1, "minute_delay": 30,
            "customer_sources": [" Leads", "check-in", "leads"] } }),
        )
        .await;
    res.assert_status(StatusCode::CREATED);
    let minutes: Value = res.json();
    assert_eq!(minutes["minute_delay"], 30);
    assert_eq!(minutes["customer_sources"], json!(["leads", "check-in"]));
    server
        .post("/v1/sms-templates")
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "name": "Bad", "body": "y", "scope": "company", "drip": {
            "lead_list_id": 101, "hour_delay": 2, "minute_delay": 60 } }),
        )
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn owners_and_admins_edit_and_delete(pool: MySqlPool) {
    with_cloudtalk(&pool).await;
    sqlx::query(
        "INSERT INTO scheduled_sms (template_id, deal_id, customer_id, user_id, company_id, send_at)
         VALUES (8671, 8000, 1000, 100, 100, NOW() + INTERVAL 1 DAY)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());
    let edited: Value = server
        .put("/v1/sms-templates/8672")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Renamed", "body": "New" }))
        .await
        .json();
    assert_eq!(edited["name"], "Renamed");
    // Mark's and the company's are not Rita's to change.
    for id in [8673, 8671] {
        server
            .put(&format!("/v1/sms-templates/{id}"))
            .with_session(sessions::REP)
            .json(&json!({ "name": "x", "body": "y" }))
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    // Keeping the drip leaves its texts; removing it cancels them.
    let kept: Value = server
        .put("/v1/sms-templates/8671")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Quoted", "body": "Hi" }))
        .await
        .json();
    assert_eq!(kept["lead_list_id"], 101);
    let pending = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT CAST(status AS CHAR) FROM scheduled_sms WHERE template_id = 8671",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert_eq!(pending().await, "pending");
    let stopped: Value = server
        .put("/v1/sms-templates/8671")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Quoted", "body": "Hi", "drip": { "lead_list_id": null, "hour_delay": null } }))
        .await
        .json();
    assert_eq!(stopped["lead_list_id"], Value::Null);
    assert_eq!(pending().await, "cancelled");

    server
        .delete("/v1/sms-templates/8672")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete("/v1/sms-templates/8672")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
