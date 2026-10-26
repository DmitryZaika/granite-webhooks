use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Calendars 9700 (shown) and 9701 (hidden from Rita); events in April 2026:
/// a one-off on 9700 for customer 1000, one on 9701, a weekly event from
/// March, one in May, and one of company 101.
async fn seed(pool: &MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO calendars (id, company_id, name, color) VALUES
           (9700, 100, 'Crew A', 'blue'), (9701, 100, 'Crew B', 'red');
         INSERT INTO calendar_hidden_users (calendar_id, user_id, company_id) VALUES (9701, 100, 100);
         INSERT INTO events (id, title, start_date, end_date, created_user_id, calendar_id, rrule, recurrence_until, customer_id) VALUES
           (9710, 'Install', '2026-04-10 15:00:00', '2026-04-10 16:00:00', 100, 9700, NULL, NULL, 1000),
           (9711, 'Hidden', '2026-04-11 15:00:00', '2026-04-11 16:00:00', 100, 9701, NULL, NULL, NULL),
           (9712, 'Weekly', '2026-03-02 15:00:00', '2026-03-02 16:00:00', 100, NULL, 'FREQ=WEEKLY', NULL, NULL),
           (9713, 'May', '2026-05-10 15:00:00', '2026-05-10 16:00:00', 100, 9700, NULL, NULL, NULL),
           (9714, 'Other', '2026-04-10 15:00:00', '2026-04-10 16:00:00', 106, NULL, NULL, NULL, NULL);
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES
           (101, 6, 100), (104, 11, 100), (104, 6, 100);
         INSERT INTO user_schedule_colors (company_id, user_id, color) VALUES (100, 101, 'teal');",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn the_schedule_lists_a_ranges_events(pool: MySqlPool) {
    seed(&pool).await;
    let server = server(pool);
    let april = "/v1/schedule/events?start=2026-04-01T00:00:00Z&end=2026-04-30T23:59:59Z";
    let body = server
        .get(april)
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(common::ids(&body["events"]), [9712, 9710]);
    assert_eq!(body["events"][1]["title"], "Install");
    assert_eq!(body["events"][1]["customer_id"], 1000);
    assert_eq!(body["events"][1]["customer_name"], "Alice Lead");
    assert_eq!(body["events"][1]["start_date"], "2026-04-10T15:00:00Z");
    assert_eq!(body["events"][0]["rrule"], "FREQ=WEEKLY");
    assert_eq!(body["quickbooks_connected"], false);
    // The admin is not hidden from Crew B.
    let admin = server
        .get(april)
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(common::ids(&admin["events"]), [9712, 9710, 9711]);
    server
        .get("/v1/schedule/events?start=2026-04-30T00:00:00Z&end=2026-04-01T00:00:00Z")
        .with_session(sessions::REP)
        .await
        .assert_status_bad_request();
}

#[test_db::test(seed)]
async fn admins_color_schedule_people_and_hide_calendars(pool: MySqlPool) {
    seed(&pool).await;
    let server = server(pool);
    let people = server
        .get("/v1/schedule/people")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(
        people,
        json!([
            { "id": 101, "name": "Mark Manager", "role": "installer", "color": "teal" },
            { "id": 104, "name": "Vic Viewer", "role": "installer", "color": "cyan" },
            { "id": 104, "name": "Vic Viewer", "role": "template", "color": "cyan" },
        ])
    );
    let color = |user: i32, color: &str, session: &'static str| {
        server
            .put(&format!("/v1/schedule/people/{user}/color"))
            .with_session(session)
            .json(&json!({ "color": color }))
    };
    color(104, "pink", sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    color(104, "beige", sessions::ADMIN)
        .await
        .assert_status_bad_request();
    color(100, "pink", sessions::ADMIN)
        .await
        .assert_status_bad_request();
    color(104, "pink", sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let people = server
        .get("/v1/schedule/people")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(people[1]["color"], "pink");

    // Hidden calendars: the admin list has them, with who they are hidden from.
    let all = server
        .get("/v1/calendars?all=true")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    let ids: Vec<i64> = common::ids(&all);
    assert!(ids.contains(&9700) && ids.contains(&9701));
    server
        .get("/v1/calendars?all=true")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .put("/v1/calendars/9701/hidden-users/100")
        .with_session(sessions::ADMIN)
        .json(&json!({ "hidden": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .put("/v1/calendars/9700/hidden-users/101")
        .with_session(sessions::ADMIN)
        .json(&json!({ "hidden": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .put("/v1/calendars/9700/hidden-users/106")
        .with_session(sessions::ADMIN)
        .json(&json!({ "hidden": true }))
        .await
        .assert_status_not_found();
    let hidden = server
        .get("/v1/calendars/hidden-users")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(hidden, json!([{ "calendar_id": 9700, "user_id": 101 }]));

    let employees = server
        .get("/v1/users/employees")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(common::ids(&employees), [107, 101, 100]);
}

#[test_db::test(seed)]
async fn activity_lists_the_latest_changed_events_first(pool: MySqlPool) {
    seed(&pool).await;
    sqlx::raw_sql(
        "UPDATE events SET assigned_user_id = 101 WHERE id = 9710;
         INSERT INTO event_history (event_id, company_id, action, message, created_by, created_at) VALUES
           (9710, 100, 'created', 'Event created', 'Rita Rep', '2026-04-01 10:00:00'),
           (9710, 100, 'date_updated', 'Date updated', 'Rita Rep', '2026-04-02 10:00:00'),
           (9710, 100, 'label_updated', 'Label updated', 'Rita Rep', '2026-04-03 10:00:00'),
           (9710, 100, 'comment_added', 'Comment added', 'Rita Rep', '2026-04-04 09:00:00'),
           (9711, 100, 'created', 'Event created', 'Rita Rep', '2026-04-05 10:00:00'),
           (9713, 100, 'created', 'Event created', 'Mark Manager', '2026-04-03 12:00:00'),
           (9714, 101, 'created', 'Event created', 'Other', '2026-04-06 10:00:00');
         INSERT INTO event_comments (event_id, company_id, content, created_by, created_at) VALUES
           (9710, 100, 'On my way', 'Mark Manager', '2026-04-04 09:00:00');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let body = server
        .get("/v1/schedule/activity")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    // Crew B is hidden from Rita, and 9714 is another company's.
    assert_eq!(body["has_more"], false);
    assert_eq!(body["events"].as_array().unwrap().len(), 2);
    let first = &body["events"][0];
    assert_eq!(first["event"]["id"], 9710);
    assert_eq!(first["event"]["customer_name"], "Alice Lead");
    assert_eq!(first["assignee_name"], "Mark Manager");
    assert_eq!(first["unscheduled"], false);
    assert_eq!(first["last_activity_at"], "2026-04-04T09:00:00Z");
    let messages: Vec<&str> = first["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line["message"].as_str().unwrap())
        .collect();
    assert_eq!(
        messages,
        ["Date updated", "Label updated", "Event commented"]
    );
    assert_eq!(body["events"][1]["event"]["id"], 9713);

    let page = server
        .get("/v1/schedule/activity?limit=1&offset=1")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(page["events"][0]["event"]["id"], 9710);
    assert_eq!(page["has_more"], true);
}

#[test_db::test(seed)]
async fn admins_pick_the_day_weeks_start_on(pool: MySqlPool) {
    let server = server(pool);
    let settings = |session: &'static str| {
        let request = server.get("/v1/schedule/settings").with_session(session);
        async move { request.await.json::<Value>() }
    };
    let set = |session: &'static str, day: &str| {
        server
            .put("/v1/schedule/settings")
            .with_session(session)
            .json(&json!({ "week_starts_on": day }))
    };
    assert_eq!(settings(sessions::REP).await["week_starts_on"], "sunday");
    set(sessions::REP, "monday")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    set(sessions::ADMIN, "friday")
        .await
        .assert_status_bad_request();
    set(sessions::ADMIN, "monday")
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(settings(sessions::REP).await["week_starts_on"], "monday");
    // Another company keeps its own.
    assert_eq!(
        settings(sessions::OTHER_COMPANY_REP).await["week_starts_on"],
        "sunday"
    );
}
