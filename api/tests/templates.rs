mod common;

use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: company 100 has the company template 8670 ("Quoted drip"). Dina (107)
// and Rita (100) are its employees, Mark (101) its manager, Olga (106) is in
// company 101.

const DINA: &str = sessions::DEALER;
const RITA: &str = sessions::REP;

async fn create(server: &TestServer, session: &str, name: &str, scope: Option<&str>) -> Value {
    let res = server
        .post("/v1/email-templates")
        .with_session(session)
        .json(&json!({
            "template_name": name,
            "template_subject": format!("{name} subject"),
            "template_body": "<p>Hi {{customer.first_name}}</p>",
            "scope": scope,
        }))
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json()
}

fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|t| t["template_name"].as_str().unwrap().to_string())
        .collect()
}

async fn list(server: &TestServer, session: &str) -> Value {
    let res = server
        .get("/v1/email-templates")
        .with_session(session)
        .await;
    res.assert_status_ok();
    res.json()
}

#[test_db::test(seed)]
async fn lists_company_templates_and_only_own_personal_ones(pool: MySqlPool) {
    let server = server(pool);
    let mine = create(&server, DINA, "Mine", None).await;
    assert_eq!(mine["scope"], "personal");
    assert_eq!(mine["can_edit"], true);
    assert_eq!(mine["template_body"], "<p>Hi {{customer.first_name}}</p>");
    create(&server, RITA, "Rita's", None).await;

    // Company templates first, then newest first.
    let dina = list(&server, DINA).await;
    assert_eq!(names(&dina), ["Quoted drip", "Mine"]);
    assert_eq!(dina[0]["scope"], "company");
    assert_eq!(dina[0]["can_edit"], false);
    assert_eq!(dina[0]["attachments"], json!([]));
    assert_eq!(names(&list(&server, RITA).await), ["Quoted drip", "Rita's"]);
    assert_eq!(list(&server, sessions::OTHER_COMPANY_REP).await, json!([]));
}

#[test_db::test(seed)]
async fn only_admins_create_company_templates(pool: MySqlPool) {
    let server = server(pool);
    server
        .post("/v1/email-templates")
        .with_session(DINA)
        .json(&json!({
            "template_name": "All", "template_subject": "S", "template_body": "B", "scope": "company"
        }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let company = create(&server, sessions::ADMIN, "For everyone", Some("company")).await;
    assert_eq!(company["scope"], "company");
    assert_eq!(company["can_edit"], false);
    assert!(names(&list(&server, DINA).await).contains(&"For everyone".to_string()));
}

#[test_db::test(seed)]
async fn invalid_templates_are_refused(pool: MySqlPool) {
    let server = server(pool);
    for (body, error) in [
        (
            json!({ "template_name": " ", "template_subject": "S", "template_body": "B" }),
            "Template name is required",
        ),
        (
            json!({ "template_name": "N", "template_subject": "", "template_body": "B" }),
            "Template subject is required",
        ),
        (
            json!({ "template_name": "N", "template_subject": "S", "template_body": "Hi {{customer.first_name}" }),
            "Invalid template body format. Check for unclosed {{ or }}.",
        ),
    ] {
        let res = server
            .post("/v1/email-templates")
            .with_session(DINA)
            .json(&body)
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], error);
    }
}

#[test_db::test(seed)]
async fn own_templates_are_edited_and_deleted(pool: MySqlPool) {
    let server = server(pool.clone());
    let mine = create(&server, DINA, "Mine", None).await;
    let id = mine["id"].as_i64().unwrap();
    let res = server
        .put(&format!("/v1/email-templates/{id}"))
        .with_session(DINA)
        .json(&json!({ "template_name": " Renamed ", "template_subject": "New", "template_body": "Body" }))
        .await;
    res.assert_status_ok();
    let updated: Value = res.json();
    assert_eq!(updated["template_name"], "Renamed");
    assert_eq!(updated["template_subject"], "New");

    // Someone else's, and company templates, are not the caller's to change.
    let edit = json!({ "template_name": "X", "template_subject": "X", "template_body": "X" });
    server
        .put(&format!("/v1/email-templates/{id}"))
        .with_session(RITA)
        .json(&edit)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put("/v1/email-templates/8670")
        .with_session(sessions::ADMIN)
        .json(&edit)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete(&format!("/v1/email-templates/{id}"))
        .with_session(RITA)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    server
        .delete(&format!("/v1/email-templates/{id}"))
        .with_session(DINA)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(names(&list(&server, DINA).await), ["Quoted drip"]);
    let deleted: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM email_templates WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted.is_some());
}

#[test_db::test(seed)]
async fn picker_order_is_by_use_or_saved(pool: MySqlPool) {
    let server = server(pool.clone());
    let a = create(&server, DINA, "A", None).await["id"]
        .as_i64()
        .unwrap();
    let b = create(&server, DINA, "B", None).await["id"]
        .as_i64()
        .unwrap();
    assert_eq!(names(&list(&server, DINA).await), ["Quoted drip", "B", "A"]);

    // Automatic: the most used first.
    sqlx::query(
        "INSERT INTO user_template_usage (user_id, template_type, template_id, usage_count)
         VALUES (107, 'email', ?, 3)",
    )
    .bind(a)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(names(&list(&server, DINA).await), ["A", "Quoted drip", "B"]);

    let order = server
        .get("/v1/me/template-order/email")
        .with_session(DINA)
        .await;
    assert_eq!(order.json::<Value>()["mode"], "automatic");

    server
        .put("/v1/me/template-order/email")
        .with_session(DINA)
        .json(&json!({ "template_ids": [b, 8670, a] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(names(&list(&server, DINA).await), ["B", "Quoted drip", "A"]);
    let order = server
        .get("/v1/me/template-order/email")
        .with_session(DINA)
        .await;
    assert_eq!(order.json::<Value>()["mode"], "custom");
    // Another user's order is their own.
    assert_eq!(names(&list(&server, RITA).await), ["Quoted drip"]);

    server
        .delete("/v1/me/template-order/email")
        .with_session(DINA)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(names(&list(&server, DINA).await), ["A", "Quoted drip", "B"]);

    server
        .put("/v1/me/template-order/sms")
        .with_session(DINA)
        .json(&json!({ "template_ids": [] }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .put("/v1/me/template-order/quote")
        .with_session(DINA)
        .json(&json!({ "template_ids": [1, 2] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let quote = server
        .get("/v1/me/template-order/quote")
        .with_session(DINA)
        .await;
    assert_eq!(quote.json::<Value>()["mode"], "custom");
    server
        .get("/v1/me/template-order/other")
        .with_session(DINA)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn fill_uses_the_caller_and_the_customer(pool: MySqlPool) {
    let server = server(pool);
    let fill = |body: Value| {
        let server = &server;
        async move {
            let res = server
                .post("/v1/templates/fill")
                .with_session(DINA)
                .json(&body)
                .await;
            res.assert_status_ok();
            res.json::<Value>()["text"].as_str().unwrap().to_string()
        }
    };
    assert_eq!(
        fill(json!({ "template": "Hi {{customer.first_name}}, {{user.name}} here", "customer_id": 8695 })).await,
        "Hi Nina, Dina Dealer here"
    );
    assert_eq!(
        fill(json!({ "template": "Hi {{customer.first_name}}", "recipient_email": "Nina <NINA@example.com>" })).await,
        "Hi Nina"
    );
    assert_eq!(
        fill(json!({ "template": "No variables" })).await,
        "No variables"
    );
    server
        .post("/v1/templates/fill")
        .json(&json!({ "template": "x" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}
