mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Company 100: template 8671 is a company SMS template; 9301 is Rita's own,
// 9302 Mark's own. Flow 9400 is the company's, 9401 Rita's, 9402 Mark's, 9403
// an AI flow of Rita's. Rita has an active enrollment in 9401.

const FIXTURE: &str = "
INSERT INTO cloudtalk_sms_templates (id, company_id, user_id, name, body, position)
  VALUES (9301, 100, 100, 'Rita tpl', 'Rita body', 0), (9302, 100, 101, 'Mark tpl', 'Mark body', 0);
INSERT INTO sms_flows (id, company_id, user_id, name, created_at) VALUES
  (9400, 100, NULL, 'Company flow', '2026-01-01 00:00:00'),
  (9401, 100, 100, 'Rita flow', '2026-01-02 00:00:00'),
  (9402, 100, 101, 'Mark flow', '2026-01-03 00:00:00'),
  (9403, 100, 100, 'Generated with AI', '2026-01-04 00:00:00');
INSERT INTO sms_flow_steps (flow_id, position, channel, template_id, custom_text, delay_minutes) VALUES
  (9400, 1, 'sms', 8671, NULL, 0),
  (9401, 1, 'sms', NULL, 'Hi {{customer.first_name}}', 0),
  (9401, 2, 'sms', 9301, NULL, 1440);
INSERT INTO sms_flow_enrollments (id, flow_id, company_id, customer_phone_digits, user_id, status,
                                  current_step_position, anchor_at, next_send_at)
  VALUES (9500, 9401, 100, 3175550100, 100, 'active', 2, '2026-07-28 14:15:00', '2026-07-29 14:15:00');
";

async fn seed(pool: &MySqlPool) {
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
}

fn names(body: &Value) -> Vec<&str> {
    body["flows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn reps_list_company_and_own_flows(pool: MySqlPool) {
    seed(&pool).await;
    let server = server(pool);
    let res = server
        .get("/v1/sms-flows")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(names(&body), ["Company flow", "Rita flow"]);
    assert_eq!(body["can_manage_company"], false);
    let company = &body["flows"][0];
    assert_eq!(company["can_edit"], false);
    assert_eq!(company["steps"][0]["template_name"], "Quoted sms");
    assert_eq!(company["steps"][0]["template_body"], "Hi");
    let rita = &body["flows"][1];
    assert_eq!(rita["scope"], "personal");
    assert_eq!(rita["can_edit"], true);
    assert_eq!(rita["steps"][1]["delay_minutes"], 1440);

    // The AI flow opens by id; Mark's does not.
    let ai: Value = server
        .get("/v1/sms-flows/9403")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(ai["name"], "Generated with AI");
    server
        .get("/v1/sms-flows/9402")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let admin: Value = server
        .get("/v1/sms-flows")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(admin["can_manage_company"], true);
    assert_eq!(admin["flows"][0]["can_edit"], true);
}

#[test_db::test(seed)]
async fn flows_are_created_with_their_templates_checked(pool: MySqlPool) {
    seed(&pool).await;
    let server = server(pool.clone());
    let post = |session: &'static str, body: Value| {
        let request = server
            .post("/v1/sms-flows")
            .with_session(session)
            .json(&body);
        async move { request.await }
    };
    let res = post(
        sessions::REP,
        json!({ "name": " Nurture ", "scope": "personal", "steps": [
            { "template_id": 9301, "delay_minutes": 0 },
            { "channel": "email", "custom_text": "<p>Still there?</p>", "subject": "Hi", "delay_minutes": 2880 },
        ] }),
    )
    .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    let flow: Value = server
        .get(&format!("/v1/sms-flows/{id}"))
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(flow["name"], "Nurture");
    assert_eq!(flow["steps"][1]["channel"], "email");
    assert_eq!(flow["steps"][1]["subject"], "Hi");

    for (session, body, status, token) in [
        // Mark's template is not Rita's to use.
        (
            sessions::REP,
            json!({ "name": "x", "scope": "personal", "steps": [{ "template_id": 9302, "delay_minutes": 0 }] }),
            400,
            "template_not_found",
        ),
        // A company flow may only use company templates.
        (
            sessions::ADMIN,
            json!({ "name": "x", "scope": "company", "steps": [{ "template_id": 9301, "delay_minutes": 0 }] }),
            400,
            "template_not_found",
        ),
        (
            sessions::REP,
            json!({ "name": "x", "scope": "company", "steps": [{ "template_id": 8671, "delay_minutes": 0 }] }),
            403,
            "forbidden",
        ),
        (
            sessions::REP,
            json!({ "name": "x", "scope": "personal", "steps": [] }),
            400,
            "validation_failed",
        ),
        (
            sessions::REP,
            json!({ "name": "x", "scope": "personal", "steps": [{ "custom_text": "Hi {{pet}}", "delay_minutes": 0 }] }),
            400,
            "template_has_custom_variables",
        ),
    ] {
        let res = post(session, body).await;
        res.assert_status(StatusCode::from_u16(status).unwrap());
        assert_eq!(res.json::<Value>()["error"], token);
    }
}

#[test_db::test(seed)]
async fn editing_reschedules_and_deleting_cancels(pool: MySqlPool) {
    seed(&pool).await;
    let server = server(pool.clone());
    server
        .put("/v1/sms-flows/9401")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Rita flow", "steps": [
            { "custom_text": "Hi", "delay_minutes": 0 },
            { "custom_text": "Again", "delay_minutes": 2880 },
        ] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // Anchored Tue 10:15 EDT, the second step is now two days later.
    let next: chrono::NaiveDateTime =
        sqlx::query_scalar("SELECT next_send_at FROM sms_flow_enrollments WHERE id = 9500")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(next.to_string(), "2026-07-30 14:15:00");

    for (id, session, status) in [
        (9402, sessions::REP, StatusCode::FORBIDDEN),
        (9400, sessions::REP, StatusCode::FORBIDDEN),
        (9999, sessions::REP, StatusCode::NOT_FOUND),
    ] {
        server
            .put(&format!("/v1/sms-flows/{id}"))
            .with_session(session)
            .json(&json!({ "name": "x", "steps": [{ "custom_text": "x", "delay_minutes": 0 }] }))
            .await
            .assert_status(status);
    }

    server
        .delete("/v1/sms-flows/9401")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let status: String =
        sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM sms_flow_enrollments WHERE id = 9500")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "cancelled");
    let listed: Value = server
        .get("/v1/sms-flows")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(names(&listed), ["Company flow"]);
}
