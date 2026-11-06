use crate::common;

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
        json!({ "events": [{ "type": "task.sighted", "at": "2026-05-01T10:00:00Z", "taskId": "" }] }),
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

fn task_position(at: &str, extra: Value) -> Value {
    let mut event = json!({
        "type": "task.position", "at": at, "taskId": "dashboard-overview",
        "version": 2, "stepKey": "employee-dashboard-title"
    });
    if let (Some(event), Some(extra)) = (event.as_object_mut(), extra.as_object()) {
        event.extend(extra.clone());
    }
    event
}

fn finish_tour(at: &str) -> Value {
    json!([
        { "type": "task.step_done", "at": at, "taskId": "dashboard-overview", "version": 2,
          "steps": { "employee-dashboard-title": "action", "employee-dashboard-edit": "action" } },
        { "type": "task.completed", "at": at, "taskId": "dashboard-overview", "version": 2 }
    ])
}

#[test_db::test(seed)]
async fn a_usage_skipped_tour_pays_nothing_and_a_paid_one_keeps_its_points(pool: MySqlPool) {
    let server = server(pool.clone());
    tours_on(&pool).await;
    let mut events = vec![
        task_position(
            "2026-05-01T10:00:00.000Z",
            json!({ "usageSkipped": true, "trigger": "gesture" }),
        ),
        // The first trigger sticks; a second skip keeps the first stamp.
        task_position(
            "2026-05-01T10:00:30.000Z",
            json!({ "usageSkipped": true, "trigger": "user" }),
        ),
    ];
    events.extend(
        finish_tour("2026-05-01T10:01:00.000Z")
            .as_array()
            .unwrap()
            .clone(),
    );
    let done: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::REP)
        .json(&post_events(json!(events)))
        .await
        .json();
    let tour = &done["state"]["tasks"]["dashboard-overview"];
    assert_eq!(tour["status"], "completed");
    assert_eq!(tour["points_awarded"], 0);
    assert_eq!(tour["start_trigger"], "gesture");
    assert_eq!(tour["usage_skipped_at"], "2026-05-01T10:00:00.000Z");
    // Repricing on a later event keeps it at 0.
    let later: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::REP)
        .json(&post_events(json!([
            { "type": "task.closed", "at": "2026-05-01T11:00:00.000Z", "taskId": "dashboard-arrange", "version": 2, "stepKey": null }
        ])))
        .await
        .json();
    assert_eq!(later["state"]["points"], 0);

    // A tour already completed and paid is not unpaid by a skip.
    let mut paid = finish_tour("2026-05-01T10:00:00.000Z")
        .as_array()
        .unwrap()
        .clone();
    paid.push(task_position(
        "2026-05-01T12:00:00.000Z",
        json!({ "usageSkipped": true }),
    ));
    let kept: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::MANAGER)
        .json(&post_events(json!(paid)))
        .await
        .json();
    let tour = &kept["state"]["tasks"]["dashboard-overview"];
    assert_eq!(tour["status"], "completed");
    assert_eq!(tour["points_awarded"], 5);
    assert!(tour["usage_skipped_at"].is_null(), "{tour}");
}

fn dashboard_overview_checks_quotes(task: &str) -> Option<String> {
    (task == "dashboard-overview").then(|| "quote_emailed".to_string())
}

async fn seed_usage(pool: &MySqlPool, user: i32, first_used: &str, detected: &str) {
    sqlx::query(
        "INSERT INTO user_feature_usage (user_id, company_id, check_id, first_used_at, source, detected_at)
         VALUES (?, 100, 'quote_emailed', ?, 'data', ?)",
    )
    .bind(user)
    .bind(first_used)
    .bind(detected)
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn a_tour_created_after_old_use_is_stamped_used(pool: MySqlPool) {
    use api::routes::onboarding::store::{Actor, Applied, OnboardingEvent, apply_events_checked};
    tours_on(&pool).await;
    // User 100 used it an hour before the tour opened; user 101 five minutes
    // before (that may be the tour's own walkthrough).
    seed_usage(&pool, 100, "2026-05-01 09:00:00", "2026-05-01 09:30:00").await;
    seed_usage(&pool, 101, "2026-05-01 09:55:00", "2026-05-01 09:56:00").await;
    for user in [100, 101] {
        let mut events = vec![task_position("2026-05-01T10:00:00.000Z", json!({}))];
        events.extend(
            finish_tour("2026-05-01T10:01:00.000Z")
                .as_array()
                .unwrap()
                .clone(),
        );
        let events: Vec<OnboardingEvent> = serde_json::from_value(json!(events)).unwrap();
        let actor = Actor {
            user_id: user,
            company_id: 100,
        };
        let applied = apply_events_checked(
            &pool,
            actor,
            &events,
            false,
            dashboard_overview_checks_quotes,
        )
        .await
        .unwrap();
        assert!(matches!(applied, Applied::Done { .. }));
    }
    let server = server(pool);
    let old: Value = server
        .get("/v1/me/onboarding")
        .with_session(sessions::REP)
        .await
        .json();
    let tour = &old["state"]["tasks"]["dashboard-overview"];
    assert_eq!(tour["usage_skipped_at"], "2026-05-01T10:00:00.000Z");
    assert_eq!(tour["points_awarded"], 0);
    let recent: Value = server
        .get("/v1/me/onboarding")
        .with_session(sessions::MANAGER)
        .await
        .json();
    let tour = &recent["state"]["tasks"]["dashboard-overview"];
    assert!(tour["usage_skipped_at"].is_null(), "{tour}");
    assert_eq!(tour["points_awarded"], 5);
}

#[test_db::test(seed)]
async fn only_event_checks_can_be_reported(pool: MySqlPool) {
    let server = server(pool.clone());
    let report = |check: &'static str| {
        server
            .post("/v1/me/onboarding/usage")
            .with_session(sessions::REP)
            .json(&json!({ "check_id": check }))
    };
    let stored = || async {
        sqlx::query_as::<_, (String, chrono::NaiveDateTime)>(
            "SELECT check_id, first_used_at FROM user_feature_usage WHERE user_id = 100",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    // Tours off: accepted and ignored.
    report("quote_views_opened")
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(stored().await.is_empty());

    tours_on(&pool).await;
    report("quote_views_opened")
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let first = stored().await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].0, "quote_views_opened");
    // Idempotent: a repeat keeps the first time.
    report("quote_views_opened")
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(stored().await, first);
    // Data checks are the server's to read; unknown ids are refused.
    report("quote_emailed")
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    report("nope").await.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(stored().await.len(), 1);
    // A check read from the tables that the app also reports is accepted.
    report("schedule_texts_saved")
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(stored().await.len(), 2);
}

/// The seed gives the rep CloudTalk calls this month, which `customer_called`
/// counts; tests that pin the whole usage map start without them.
async fn without_seeded_calls(pool: &MySqlPool) {
    sqlx::query("DELETE FROM phone_call_points WHERE company_id = 100")
        .execute(pool)
        .await
        .unwrap();
}

#[test_db::test(seed)]
async fn progress_reports_usage_and_the_account_date(pool: MySqlPool) {
    without_seeded_calls(&pool).await;
    let server = server(pool.clone());
    seed_usage(&pool, 100, "2026-05-01 09:00:00", "2026-05-01 09:30:00").await;
    sqlx::query(
        "INSERT INTO user_feature_usage (user_id, company_id, check_id, first_used_at, source, detected_at)
         VALUES (100, 100, 'retired_check', '2026-05-01 09:00:00', 'data', '2026-05-01 09:30:00')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let off: Value = server
        .get("/v1/me/onboarding")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(off["usage"], json!({}));
    assert!(off["account_created_at"].is_string(), "{off}");

    tours_on(&pool).await;
    let on: Value = server
        .get("/v1/me/onboarding")
        .with_session(sessions::REP)
        .await
        .json();
    // Retired checks are not served.
    assert_eq!(
        on["usage"],
        json!({ "quote_emailed": "2026-05-01T09:00:00.000Z" })
    );
    // A feature-tasks reset after the use drops it.
    sqlx::query(
        "INSERT INTO user_onboarding (user_id, company_id, feature_tasks_reset_at)
         VALUES (100, 100, '2026-05-02 00:00:00')
         ON DUPLICATE KEY UPDATE feature_tasks_reset_at = VALUES(feature_tasks_reset_at)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let reset: Value = server
        .get("/v1/me/onboarding")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(reset["usage"], json!({}));
}

#[test_db::test(seed)]
async fn data_checks_read_the_company_and_respect_the_fence(pool: MySqlPool) {
    without_seeded_calls(&pool).await;
    use api::routes::onboarding::store::Actor;
    use api::routes::onboarding::usage::{evaluate, evaluate_team};
    let all = [
        "quote_emailed",
        "schedule_event_created",
        "send_later_used",
        "templates_used",
        "signature_sent",
        "chat_asked",
        "ai_reply_used",
        "customer_called",
        "call_followed_up",
        "quote_drawn",
        "quote_cutout_added",
        "automation_set_up",
        "schedule_texts_saved",
    ];
    let rep = Actor {
        user_id: 100,
        company_id: 100,
    };
    // Every query runs, and nothing is positive yet.
    assert!(evaluate(&pool, rep, &all).await.unwrap().is_empty());
    assert!(
        evaluate_team(&pool, 100, &[100, 101], &all)
            .await
            .unwrap()
            .values()
            .all(|uses| uses.is_empty())
    );
    sqlx::query(
        "INSERT INTO user_template_usage (user_id, template_type, template_id, last_used_at)
         VALUES (100, 'email', 1, '2026-05-01 09:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO delayed_emails (company_id, user_id, recipient_emails, send_at, baseline_at, payload_json, created_at)
         VALUES (100, 101, 'a@b.c', '2026-05-02 09:00:00', '2026-05-01 09:00:00', '{}', '2026-05-01 08:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap();
    // A company follow-up flow the user created; a personal one is not.
    sqlx::query(
        "INSERT INTO sms_flows (company_id, user_id, name, created_by, created_at)
         VALUES (100, NULL, 'Company flow', 100, '2026-05-01 07:00:00'),
                (100, 101, 'My flow', 101, '2026-05-01 07:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Two saved drawings by the second user: only the later one has a sink cutout.
    sqlx::query(
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep, company_id,
                             salesperson_id, created_by, status)
         VALUES (9801, 1000, 'Kitchen', 'quote', NOW(), 'Rita Rep', 100, 101, 101, 'draft')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO quote_revisions (quote_id, revision_number, drawing_json, created_by, created_at)
           VALUES (9801, 1, '{"counters":[{"runs":[{"cutouts":[]}]}]}', 101, '2026-05-01 06:00:00'),
                  (9801, 2, '{"counters":[{"runs":[{"cutouts":[{"kind":"sink"}]}]}]}', 101, '2026-05-01 06:30:00')"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    // Other AI calls are not their feature: the rep's Schedule estimate and
    // price offers (under their own names now, and as `chat` from before
    // they got them) are no Chat question, and a call's text alone is no
    // follow-up. The second user's Chat question and call note count.
    sqlx::query(
        "INSERT INTO ai_usage_log (feature, api_kind, model, user_id, company_id, success, created_at)
         VALUES ('schedule_estimate', 'chat', 'gpt-6-luna', 100, 100, 1, '2026-05-01 05:00:00'),
                ('price_offers', 'chat', 'gpt-6-luna', 100, 100, 1, '2026-05-01 05:00:00'),
                ('chat', 'chat', 'gpt-6-luna', 100, 100, 1, '2026-10-08 05:00:00'),
                ('call_transcription', 'transcription', 'universal-3-5-pro', 100, 100, 1, '2026-05-01 05:00:00'),
                ('chat', 'chat', 'gpt-6-luna', 101, 100, 1, '2026-05-01 05:00:00'),
                ('call_transcription', 'transcription', 'universal-3-5-pro', 101, 100, 1, '2026-05-01 04:00:00'),
                ('call_note_summary', 'chat', 'gpt-6-luna', 101, 100, 1, '2026-05-01 05:30:00')",
    )
    .execute(&pool)
    .await
    .unwrap();
    // A Schedule text template the second user made (deleted since).
    sqlx::query(
        "INSERT INTO schedule_notification_templates (company_id, name, channel, body, created_by, created_at, deleted_at)
         VALUES (100, 'Reminder', 'sms', 'See you soon', 101, '2026-05-01 03:00:00', '2026-05-01 03:30:00')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let team = evaluate_team(&pool, 100, &[100, 101], &all).await.unwrap();
    assert!(!team[&100].contains_key("chat_asked"), "{:?}", team[&100]);
    assert!(
        !team[&100].contains_key("call_followed_up"),
        "{:?}",
        team[&100]
    );
    assert_eq!(team[&101]["chat_asked"], "2026-05-01T05:00:00.000Z");
    assert_eq!(team[&101]["call_followed_up"], "2026-05-01T05:30:00.000Z");
    assert_eq!(
        team[&101]["schedule_texts_saved"],
        "2026-05-01T03:00:00.000Z"
    );
    assert!(!team[&100].contains_key("schedule_texts_saved"));
    assert_eq!(team[&101]["quote_drawn"], "2026-05-01T06:00:00.000Z");
    assert_eq!(team[&101]["quote_cutout_added"], "2026-05-01T06:30:00.000Z");
    assert!(!team[&100].contains_key("quote_drawn"));
    assert_eq!(team[&100]["templates_used"], "2026-05-01T09:00:00.000Z");
    assert_eq!(team[&100]["automation_set_up"], "2026-05-01T07:00:00.000Z");
    assert!(!team[&101].contains_key("automation_set_up"));
    assert_eq!(team[&101]["send_later_used"], "2026-05-01T08:00:00.000Z");
    assert!(!team[&100].contains_key("send_later_used"));
    // One user's check finds the same, from the cache the team run filled.
    let own = evaluate(&pool, rep, &all).await.unwrap();
    assert_eq!(own.len(), 2);
    // A reset after the use drops the cached positive, and the data no
    // longer counts.
    sqlx::query("INSERT INTO user_onboarding (user_id, company_id, feature_tasks_reset_at) VALUES (100, 100, '2026-05-02 00:00:00') ON DUPLICATE KEY UPDATE feature_tasks_reset_at = VALUES(feature_tasks_reset_at)")
        .execute(&pool)
        .await
        .unwrap();
    assert!(evaluate(&pool, rep, &all).await.unwrap().is_empty());
}

fn sighted(at: &str) -> Value {
    post_events(json!([
        { "type": "task.sighted", "at": at, "taskId": "dashboard-overview" }
    ]))
}

async fn my_progress(server: &axum_test::TestServer) -> Value {
    server
        .get("/v1/me/onboarding")
        .with_session(sessions::REP)
        .await
        .json()
}

#[test_db::test(seed)]
async fn a_sighting_is_saved_once_and_is_not_tour_progress(pool: MySqlPool) {
    tours_on(&pool).await;
    let server = server(pool.clone());
    let first: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::REP)
        .json(&sighted("2026-05-01T10:00:00.000Z"))
        .await
        .json();
    assert_eq!(
        first["state"]["sightings"],
        json!({ "dashboard-overview": "2026-05-01T10:00:00.000Z" })
    );
    // A replay with a later time keeps the first one.
    let replay: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::REP)
        .json(&sighted("2026-05-02T10:00:00.000Z"))
        .await
        .json();
    assert_eq!(
        replay["state"]["sightings"]["dashboard-overview"],
        "2026-05-01T10:00:00.000Z"
    );
    let progress = my_progress(&server).await;
    assert_eq!(
        progress["state"]["sightings"]["dashboard-overview"],
        "2026-05-01T10:00:00.000Z"
    );
    // It is not tour progress: no tour row, no entry in `tasks`.
    assert_eq!(progress["state"]["tasks"], json!({}));
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM user_feature_tasks WHERE user_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 0);
}

#[test_db::test(seed)]
async fn sightings_respect_the_tour_reset(pool: MySqlPool) {
    tours_on(&pool).await;
    let server = server(pool.clone());
    sqlx::query(
        "INSERT INTO user_feature_task_sightings (user_id, company_id, task_id, first_seen_at)
         VALUES (100, 100, 'dashboard-overview', '2026-05-01 10:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        my_progress(&server).await["state"]["sightings"]["dashboard-overview"],
        "2026-05-01T10:00:00.000Z"
    );
    sqlx::query("INSERT INTO user_onboarding (user_id, company_id, feature_tasks_reset_at) VALUES (100, 100, '2026-05-02 00:00:00') ON DUPLICATE KEY UPDATE feature_tasks_reset_at = VALUES(feature_tasks_reset_at)")
        .execute(&pool)
        .await
        .unwrap();
    // A sighting from before the reset is left out ...
    assert_eq!(my_progress(&server).await["state"]["sightings"], json!({}));
    // ... an event dated before it is dropped ...
    let dropped: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::REP)
        .json(&sighted("2026-05-01T12:00:00.000Z"))
        .await
        .json();
    assert_eq!(dropped["state"]["sightings"], json!({}));
    // ... and a sighting after it replaces the stale row.
    let fresh: Value = server
        .post("/v1/me/onboarding/events")
        .with_session(sessions::REP)
        .json(&sighted("2026-05-03T09:00:00.000Z"))
        .await
        .json();
    assert_eq!(
        fresh["state"]["sightings"]["dashboard-overview"],
        "2026-05-03T09:00:00.000Z"
    );
    assert_eq!(
        my_progress(&server).await["state"]["sightings"]["dashboard-overview"],
        "2026-05-03T09:00:00.000Z"
    );
}

#[test_db::test(seed)]
async fn the_team_lists_usage_sections_and_access(pool: MySqlPool) {
    let server = server(pool.clone());
    tours_on(&pool).await;
    let team: Value = server
        .get("/v1/onboarding/team")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert!(team["capabilities"]["inbound_email"].is_boolean());
    assert!(team["capabilities"]["questionnaires"].is_boolean());
    for member in team["members"].as_array().unwrap() {
        assert!(member["usage"].is_object(), "{member}");
        assert!(member["account_created_at"].is_string(), "{member}");
        for flag in ["phone_line", "message_composer", "signature_access"] {
            assert!(member[flag].is_boolean(), "{flag}: {member}");
        }
        assert!(member["sections"].is_null() || member["sections"].is_array());
    }
}
