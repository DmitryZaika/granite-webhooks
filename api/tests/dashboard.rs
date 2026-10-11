mod common;

use api::auth::cookie::SessionData;
use axum::http::StatusCode;
use common::{WithSession, cookie_for, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const WIDGETS: &str = "/v1/me/dashboard/widgets";
const EVENTS: &str = "/v1/me/dashboard/widget-events";

async fn events(pool: &MySqlPool) -> Vec<(i32, i32, String, String, String)> {
    sqlx::query_as(
        "SELECT user_id, company_id, widget_id, action, target
           FROM widget_analytics_events ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

fn event(
    user: i32,
    company: i32,
    widget: &str,
    action: &str,
    target: &str,
) -> (i32, i32, String, String, String) {
    (user, company, widget.into(), action.into(), target.into())
}

// ---- GET/PUT /v1/me/dashboard/widgets ---------------------------------------

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn a_saved_layout_reads_back_in_order(pool: MySqlPool) {
    let server = server(pool.clone());
    let empty: Value = server.get(WIDGETS).with_session(sessions::REP).await.json();
    assert_eq!(empty, json!({ "widget_ids": [] }));

    let saved: Value = server
        .put(WIDGETS)
        .with_session(sessions::REP)
        .json(&json!({ "widget_ids": ["leaderboard", "my-sales", "quick-links", "my-sales"] }))
        .await
        .json();
    // A repeated widget keeps its first place.
    let layout = json!({ "widget_ids": ["leaderboard", "my-sales", "quick-links"] });
    assert_eq!(saved, layout);
    let read: Value = server.get(WIDGETS).with_session(sessions::REP).await.json();
    assert_eq!(read, layout);

    // The first save has nothing to compare with: no events.
    assert!(events(&pool).await.is_empty());

    // A reorder records nothing; removing and adding records both.
    server
        .put(WIDGETS)
        .with_session(sessions::REP)
        .json(&json!({ "widget_ids": ["leaderboard", "quick-links", "my-sales"] }))
        .await
        .assert_status_ok();
    assert!(events(&pool).await.is_empty());
    server
        .put(WIDGETS)
        .with_session(sessions::REP)
        .json(&json!({ "widget_ids": ["quick-links", "top-sold-colors"] }))
        .await
        .assert_status_ok();
    assert_eq!(
        events(&pool).await,
        [
            event(100, 100, "top-sold-colors", "add", "add"),
            event(100, 100, "leaderboard", "remove", "remove"),
            event(100, 100, "my-sales", "remove", "remove"),
        ]
    );
    let read: Value = server.get(WIDGETS).with_session(sessions::REP).await.json();
    assert_eq!(
        read,
        json!({ "widget_ids": ["quick-links", "top-sold-colors"] })
    );

    // An empty layout is saved too.
    server
        .put(WIDGETS)
        .with_session(sessions::REP)
        .json(&json!({ "widget_ids": [] }))
        .await
        .assert_status_ok();
    let read: Value = server.get(WIDGETS).with_session(sessions::REP).await.json();
    assert_eq!(read, json!({ "widget_ids": [] }));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn layouts_are_per_user_and_company(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .put(WIDGETS)
        .with_session(sessions::REP)
        .json(&json!({ "widget_ids": ["my-sales"] }))
        .await
        .assert_status_ok();
    let other: Value = server
        .get(WIDGETS)
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(other, json!({ "widget_ids": [] }));

    // A super admin keeps one layout per company they switch into.
    let super_in = |company_id| {
        cookie_for(SessionData {
            session_id: Some(sessions::SUPER_ADMIN.into()),
            active_company_id: Some(company_id),
        })
    };
    server
        .put(WIDGETS)
        .add_header("cookie", super_in(101))
        .json(&json!({ "widget_ids": ["onboarding"] }))
        .await
        .assert_status_ok();
    let in_100: Value = server
        .get(WIDGETS)
        .add_header("cookie", super_in(100))
        .await
        .json();
    assert_eq!(in_100, json!({ "widget_ids": [] }));
    let in_101: Value = server
        .get(WIDGETS)
        .add_header("cookie", super_in(101))
        .await
        .json();
    assert_eq!(in_101, json!({ "widget_ids": ["onboarding"] }));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn retired_widget_rows_are_skipped(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO user_dashboard_widgets (user_id, company_id, widget_id, position)
         VALUES (100, 100, 'check-list', 0), (100, 100, 'my-sales', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let read: Value = server(pool)
        .get(WIDGETS)
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(read, json!({ "widget_ids": ["my-sales"] }));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn layouts_reject_unknown_widgets(pool: MySqlPool) {
    let server = server(pool.clone());
    for body in [
        json!({ "widget_ids": ["my-sales", "check-list"] }),
        json!({ "widget_ids": "my-sales" }),
        json!({}),
    ] {
        server
            .put(WIDGETS)
            .with_session(sessions::REP)
            .json(&body)
            .expect_failure()
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    assert!(events(&pool).await.is_empty());
}

// ---- POST /v1/me/dashboard/widget-events ------------------------------------

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn widget_events_are_recorded(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .post(EVENTS)
        .with_session(sessions::REP)
        .json(&json!({ "widget_id": "won-deals", "action": "change", "target": " sales-goal " }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        events(&pool).await,
        vec![event(100, 100, "won-deals", "change", "sales-goal")]
    );

    for body in [
        json!({ "widget_id": "won-deals", "action": "change", "target": "  " }),
        json!({ "widget_id": "won-deals", "action": "change", "target": "x".repeat(65) }),
        json!({ "widget_id": "won-deals", "action": "hover", "target": "x" }),
        json!({ "widget_id": "nope", "action": "click", "target": "x" }),
    ] {
        server
            .post(EVENTS)
            .with_session(sessions::REP)
            .json(&body)
            .expect_failure()
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    assert_eq!(events(&pool).await.len(), 1);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn dashboard_routes_need_an_employee(pool: MySqlPool) {
    let server = server(pool);
    let event = json!({ "widget_id": "my-sales", "action": "click", "target": "x" });
    let layout = json!({ "widget_ids": [] });
    for session in [None, Some(sessions::VIEWER)] {
        let status = if session.is_some() {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::UNAUTHORIZED
        };
        let with = |request: axum_test::TestRequest| match session {
            Some(session) => request.with_session(session),
            None => request,
        };
        with(server.get(WIDGETS))
            .expect_failure()
            .await
            .assert_status(status);
        with(server.put(WIDGETS).json(&layout))
            .expect_failure()
            .await
            .assert_status(status);
        with(server.post(EVENTS).json(&event))
            .expect_failure()
            .await
            .assert_status(status);
    }
}
