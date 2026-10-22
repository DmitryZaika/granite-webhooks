mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn tours_on(pool: &MySqlPool) {
    sqlx::query("UPDATE company SET onboarding_enabled = 1 WHERE id = 100")
        .execute(pool)
        .await
        .unwrap();
}

fn post_events(events: Value) -> Value {
    json!({ "events": events })
}

#[test_db::test(seed)]
async fn the_first_run_journey_is_saved_and_restarted(pool: MySqlPool) {
    let server = server(pool);
    let empty: Value = server
        .get("/v1/me/onboarding")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(empty["has_row"], false);
    let saved: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::REP)
        .json(&post_events(json!([
            { "type": "journey.position", "at": "2026-05-01T10:00:00.000Z", "moduleId": "stones", "stepKey": "a" },
            { "type": "journey.module_completed", "at": "2026-05-01T10:01:00.000Z", "moduleId": "customers" },
            { "type": "journey.dismissed", "at": "2026-05-01T10:02:00.000Z" }
        ])))
        .await
        .json();
    assert_eq!(saved["state"]["current_module_id"], "stones");
    assert_eq!(saved["state"]["current_step_key"], "a");
    assert_eq!(saved["state"]["completed_module_ids"], json!(["customers"]));
    assert_eq!(saved["state"]["module_step_keys"]["stones"], "a");
    assert_eq!(saved["state"]["dismissed"], true);

    server
        .post("/v1/me/onboarding/reset")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let after: Value = server
        .get("/v1/me/onboarding")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(after["has_row"], true);
    assert_eq!(after["state"]["completed_module_ids"], json!([]));
    assert_eq!(after["state"]["dismissed"], false);
    // An event from before the restart (a stale device) is dropped.
    let stale: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::REP)
        .json(&post_events(json!([
            { "type": "journey.module_completed", "at": "2020-01-01T00:00:00.000Z", "moduleId": "deals" }
        ])))
        .await
        .json();
    assert_eq!(stale["state"]["completed_module_ids"], json!([]));
}

#[test_db::test(seed)]
async fn tours_pay_points_only_when_done_and_switched_on(pool: MySqlPool) {
    let server = server(pool.clone());
    let complete = json!([
        { "type": "task.step_done", "at": "2026-05-01T10:00:00.000Z", "taskId": "dashboard-overview",
          "version": 2, "steps": { "employee-dashboard-title": "action", "employee-dashboard-edit": "action" } },
        { "type": "task.completed", "at": "2026-05-01T10:01:00.000Z", "taskId": "dashboard-overview", "version": 2 }
    ]);
    // Tours off: task events are dropped.
    let off: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::REP)
        .json(&post_events(complete.clone()))
        .await
        .json();
    assert_eq!(off["feature_tasks_enabled"], false);
    assert_eq!(off["state"]["tasks"], json!({}));

    tours_on(&pool).await;
    let on: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::REP)
        .json(&post_events(complete.clone()))
        .await
        .json();
    assert_eq!(
        on["state"]["tasks"]["dashboard-overview"]["status"],
        "completed"
    );
    assert_eq!(
        on["state"]["tasks"]["dashboard-overview"]["points_awarded"],
        5
    );
    assert_eq!(on["state"]["points"], 5);
    // Replaying pays nothing more.
    let again: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::REP)
        .json(&post_events(complete))
        .await
        .json();
    assert_eq!(again["state"]["points"], 5);

    // A completion without its steps is worth nothing; a close after it changes nothing.
    let bare: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::MANAGER)
        .json(&post_events(json!([
            { "type": "task.completed", "at": "2026-05-01T10:01:00.000Z", "taskId": "dashboard-overview", "version": 2 },
            { "type": "task.closed", "at": "2026-05-01T10:02:00.000Z", "taskId": "dashboard-overview", "version": 2, "stepKey": null }
        ])))
        .await
        .json();
    assert_eq!(
        bare["state"]["tasks"]["dashboard-overview"]["status"],
        "completed"
    );
    assert_eq!(bare["state"]["points"], 0);

    // The client cannot complete a setup task; only the server's check can.
    let forged: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::REP)
        .json(&post_events(json!([
            { "type": "task.completed", "at": "2026-05-01T10:01:00.000Z", "taskId": "setup-email-name", "version": 1 }
        ])))
        .await
        .json();
    assert!(forged["state"]["tasks"]["setup-email-name"].is_null());
}

#[test_db::test(seed)]
async fn malformed_events_are_refused(pool: MySqlPool) {
    let server = server(pool);
    for body in [
        json!({ "events": [] }),
        json!({ "events": [{ "type": "journey.dismissed", "at": "not a date" }] }),
        json!({ "events": [{ "type": "journey.module_completed", "at": "2026-05-01T10:00:00Z", "moduleId": "nope" }] }),
        json!({ "events": [{ "type": "journey.dismissed", "at": "2026-05-01T10:00:00Z", "extra": 1 }] }),
        json!({ "events": [{ "type": "points.granted", "at": "2026-05-01T10:00:00Z" }] }),
    ] {
        let status = server
            .post("/v1/me/onboarding/events")
            .with_session(sessions::REP)
            .json(&body)
            .await
            .status_code();
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
            "{body}"
        );
    }
}

#[test_db::test(seed)]
async fn finished_setup_is_credited_once(pool: MySqlPool) {
    tours_on(&pool).await;
    sqlx::query(
        "UPDATE users SET email_name = 'Rita', email_signature = '<p>Rita</p>' WHERE id = 100",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let page: Value = server
        .get("/v1/me/onboarding/setup")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(page["setup"]["has_email_name"], true);
    assert_eq!(page["setup"]["has_email_signature"], true);
    assert_eq!(page["setup"]["has_telegram"], false);
    assert_eq!(
        page["credit"]["task_ids"],
        json!(["setup-email-name", "setup-email-signature"])
    );
    assert_eq!(page["profile"]["saved_progress"]["points"], 20);
    // Credited once.
    let again: Value = server
        .get("/v1/me/onboarding/setup")
        .with_session(sessions::REP)
        .await
        .json();
    assert!(again["credit"].is_null());
    let claimed: Value = server
        .post("/v1/me/onboarding/claim")
        .with_session(sessions::REP)
        .json(&json!({ "task_id": "setup-email-name" }))
        .await
        .json();
    assert_eq!(claimed["status"], "already-completed");
    let telegram: Value = server
        .post("/v1/me/onboarding/claim")
        .with_session(sessions::REP)
        .json(&json!({ "task_id": "setup-telegram" }))
        .await
        .json();
    assert_eq!(telegram["status"], "not-done");
}

#[test_db::test(seed)]
async fn admins_see_their_company_team_progress(pool: MySqlPool) {
    let server = server(pool.clone());
    let off: Value = server
        .get("/v1/onboarding/team")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(off["enabled"], false);
    assert_eq!(off["members"], json!([]));

    tours_on(&pool).await;
    server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::REP)
        .json(&post_events(json!([
            { "type": "task.closed", "at": "2026-05-01T10:02:00.000Z", "taskId": "dashboard-overview", "version": 2, "stepKey": null }
        ])))
        .await
        .assert_status_ok();
    server
        .get("/v1/onboarding/team")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let team: Value = server
        .get("/v1/onboarding/team")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(team["enabled"], true);
    assert_eq!(team["capabilities"]["onboarding"], true);
    let members = team["members"].as_array().unwrap();
    assert!(
        members
            .iter()
            .all(|m| m["email"].as_str().unwrap().ends_with("@acme.test"))
    );
    let rita = members.iter().find(|m| m["user_id"] == 100).unwrap();
    let tour = &rita["progress"]["tasks"]["dashboard-overview"];
    assert_eq!(tour["status"], "closed");
    assert!(tour["closed_at"].is_string(), "{tour}");
    let ada = members.iter().find(|m| m["user_id"] == 102).unwrap();
    assert_eq!(ada["is_admin"], true);
    assert_eq!(ada["progress"]["tasks"], json!({}));
}
