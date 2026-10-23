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
        .with_session(RITA)
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

#[test_db::test(seed)]
async fn any_employee_uploads_template_images(pool: MySqlPool) {
    let (server, _) = common::server_with_storage(pool);
    let res = server
        .post("/v1/uploads")
        .with_session(sessions::REP)
        .json(&json!({ "folder": "template-images", "file_name": "logo.png", "content_type": "image/png" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    assert!(
        res.json::<Value>()["url"]
            .as_str()
            .unwrap()
            .contains("template-images/")
    );
    server
        .post("/v1/uploads")
        .with_session(sessions::REP)
        .json(&json!({ "folder": "template-images", "file_name": "a.pdf", "content_type": "application/pdf" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn employees_read_stored_files_through_the_api(pool: MySqlPool) {
    let (server, storage) = common::server_with_storage(pool);
    storage
        .put("emails/a.pdf", b"%PDF-1.4".to_vec(), "application/pdf")
        .await
        .unwrap();
    storage
        .put("emails/a.html", b"<p>".to_vec(), "text/html")
        .await
        .unwrap();
    let url = storage.public_url("emails/a.pdf").unwrap();
    let res = server
        .get(&format!("/v1/uploads/file?url={}", urlencoding(&url)))
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    assert_eq!(res.as_bytes().as_ref(), b"%PDF-1.4");
    let html = storage.public_url("emails/a.html").unwrap();
    server
        .get(&format!("/v1/uploads/file?url={}", urlencoding(&html)))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .get("/v1/uploads/file?url=https%3A%2F%2Felsewhere.test%2Fa.pdf")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn files_that_run_script_are_never_uploaded_or_served(pool: MySqlPool) {
    let (server, storage) = common::server_with_storage(pool);
    for (folder, content_type) in [
        ("emails", "image/svg+xml"),
        ("emails", "text/html"),
        ("bug-reports", "application/xhtml+xml"),
        ("email-templates", "text/javascript"),
        ("quotes", "image/svg+xml"),
    ] {
        let res = server
            .post("/v1/uploads")
            .with_session(sessions::ADMIN)
            .json(&json!({ "folder": folder, "file_name": "x", "content_type": content_type }))
            .await;
        assert_eq!(res.status_code(), StatusCode::BAD_REQUEST, "{folder} {content_type}");
    }

    // An SVG already in the bucket is refused; images come back as downloads
    // that cannot render as a page of the API.
    storage
        .put(
            "emails/x.svg",
            b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>".to_vec(),
            "image/svg+xml",
        )
        .await
        .unwrap();
    storage
        .put("emails/x.png", b"png".to_vec(), "image/png")
        .await
        .unwrap();
    let svg = storage.public_url("emails/x.svg").unwrap();
    server
        .get(&format!("/v1/uploads/file?url={}", urlencoding(&svg)))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .get(&format!("/v1/stones/image-proxy?url={}", urlencoding(&svg)))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let png = storage.public_url("emails/x.png").unwrap();
    for res in [
        server
            .get(&format!("/v1/uploads/file?url={}", urlencoding(&png)))
            .with_session(sessions::REP)
            .await,
        server
            .get(&format!("/v1/stones/image-proxy?url={}", urlencoding(&png)))
            .with_session(sessions::ADMIN)
            .await,
    ] {
        res.assert_status_ok();
        assert_eq!(res.header("content-type"), "image/png");
        assert_eq!(res.header("x-content-type-options"), "nosniff");
        assert_eq!(res.header("content-security-policy"), "default-src 'none'; sandbox");
        assert_eq!(res.header("content-disposition"), "attachment");
    }
}

fn urlencoding(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn drip(name: &str, list: Option<i64>, hours: Option<i64>, scope: &str) -> Value {
    json!({
        "template_name": name,
        "template_subject": "Subject",
        "template_body": "<p>Hi</p>",
        "scope": scope,
        "lead_list_id": list,
        "hour_delay": hours,
    })
}

#[test_db::test(seed)]
async fn admins_make_company_templates_that_send_automatically(pool: MySqlPool) {
    let server = server(pool.clone());
    let res = server
        .post("/v1/email-templates")
        .with_session(sessions::ADMIN)
        .json(&drip("New lead hello", Some(100), Some(4), "company"))
        .await;
    res.assert_status(StatusCode::CREATED);
    let created: Value = res.json();
    assert_eq!(created["scope"], "company");
    assert_eq!(created["lead_list_id"], 100);
    assert_eq!(created["hour_delay"], 4);
    assert_eq!(created["list_name"], "New");
    assert_eq!(created["group_name"], "Sales");
    assert!(created["created_at"].is_string());
    let group: Option<i32> =
        sqlx::query_scalar("SELECT lead_group_id FROM email_templates WHERE id = ?")
            .bind(created["id"].as_i64().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(group, Some(100));

    // 8670 already sends on list 101 after 2 hours.
    let res = server
        .post("/v1/email-templates")
        .with_session(sessions::ADMIN)
        .json(&drip("Twin", Some(101), Some(2), "company"))
        .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        res.json::<Value>()["error"],
        "Another email template already uses this list and delay."
    );
    for (body, why) in [
        (drip("X", Some(102), Some(1), "company"), "deleted list"),
        (
            drip("X", Some(103), Some(1), "company"),
            "other company's list",
        ),
        (drip("X", Some(100), None, "company"), "no delay"),
        (
            drip("X", Some(100), Some(9000), "company"),
            "delay too long",
        ),
        (
            drip("X", Some(100), Some(1), "personal"),
            "personal template",
        ),
    ] {
        let res = server
            .post("/v1/email-templates")
            .with_session(sessions::ADMIN)
            .json(&body)
            .await;
        assert_eq!(res.status_code(), StatusCode::BAD_REQUEST, "{why}");
    }
}

async fn pending_send(pool: &MySqlPool) -> i64 {
    sqlx::query(
        "INSERT INTO scheduled_emails (template_id, deal_id, list_id, customer_id, user_id, company_id, send_at, status)
         VALUES (8670, 8602, 101, 1010, 107, 100, '2037-01-01 00:00:00', 'pending')",
    )
    .execute(pool)
    .await
    .unwrap()
    .last_insert_id()
    .try_into()
    .unwrap()
}

async fn send_status(pool: &MySqlPool, id: i64) -> String {
    sqlx::query_scalar("SELECT status FROM scheduled_emails WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn admins_edit_company_templates_and_their_automation(pool: MySqlPool) {
    let server = server(pool.clone());
    let send = pending_send(&pool).await;
    let res = server
        .put("/v1/email-templates/8670")
        .with_session(sessions::ADMIN)
        .json(&drip("Quoted drip", Some(101), Some(3), "company"))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["hour_delay"], 3);
    assert_eq!(send_status(&pool, send).await, "pending");
    // No automation any more: its pending sends are called off.
    let res = server
        .put("/v1/email-templates/8670")
        .with_session(sessions::ADMIN)
        .json(&drip("Quoted drip", None, None, "company"))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["lead_list_id"], Value::Null);
    assert_eq!(send_status(&pool, send).await, "cancelled");
}

#[test_db::test(seed)]
async fn deleting_a_company_template_stops_its_sends(pool: MySqlPool) {
    let server = server(pool.clone());
    let send = pending_send(&pool).await;
    server
        .delete("/v1/email-templates/8670")
        .with_session(RITA)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete("/v1/email-templates/8670")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(send_status(&pool, send).await, "cancelled");
    // Its list and delay are free again.
    server
        .post("/v1/email-templates")
        .with_session(sessions::ADMIN)
        .json(&drip("Again", Some(101), Some(2), "company"))
        .await
        .assert_status(StatusCode::CREATED);
}

#[test_db::test(seed)]
async fn template_attachments(pool: MySqlPool) {
    let (server, _) = common::server_with_storage(pool);
    server
        .post("/v1/uploads")
        .with_session(RITA)
        .json(&json!({ "folder": "email-templates", "file_name": "a.pdf", "content_type": "application/pdf" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let res = server
        .post("/v1/uploads")
        .with_session(sessions::ADMIN)
        .json(&json!({ "folder": "email-templates", "file_name": "terms.pdf", "content_type": "application/pdf" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let url = res.json::<Value>()["url"].as_str().unwrap().to_string();
    let attach = |url: String| json!({ "url": url, "filename": "terms.pdf", "content_type": "application/pdf" });
    let res = server
        .post("/v1/email-templates/8670/attachments")
        .with_session(sessions::ADMIN)
        .json(&attach(url.clone()))
        .await;
    res.assert_status(StatusCode::CREATED);
    let attachment: Value = res.json();
    assert_eq!(attachment["content_type"], "application");
    assert_eq!(attachment["content_subtype"], "pdf");
    server
        .post("/v1/email-templates/8670/attachments")
        .with_session(sessions::ADMIN)
        .json(&attach("https://evil.example/terms.pdf".into()))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/email-templates/8670/attachments")
        .with_session(RITA)
        .json(&attach(url))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let templates = list(&server, sessions::ADMIN).await;
    let drip = templates
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == 8670)
        .unwrap();
    assert_eq!(drip["attachments"][0]["filename"], "terms.pdf");
    let id = attachment["id"].as_i64().unwrap();
    let path = format!("/v1/email-templates/8670/attachments/{id}");
    server
        .delete(&path)
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete(&path)
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
