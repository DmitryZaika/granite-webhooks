mod common;

use api::app;
use api::auth::cookie::SessionData;
use api::state::AppState;
use api::telegram::TelegramBots;
use axum::extract::Path as AxumPath;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, cookie_for, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn me_returns_session_user(pool: MySqlPool) {
    let res = server(pool).get("/v1/me").with_session(sessions::REP).await;
    res.assert_status_ok();
    res.assert_json(&json!({
        "id": 100,
        "email": "rep@acme.test",
        "name": "Rita Rep",
        "phone_number": null,
        "is_employee": true,
        "is_admin": false,
        "is_superuser": false,
        "company_id": 100,
        "pined_bar": false,
        "cloudtalk_agent_id": null,
        "cloudtalk_phone_number": null,
        "ringcentral_extension_id": null,
        "ringcentral_phone_number": null,
        "is_signature_available": false
    }));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn positions_are_scoped_to_the_acting_company(pool: MySqlPool) {
    let server = server(pool);

    // Manager holds sales_manager in 100 and sales_rep in 101.
    server
        .get("/v1/me/positions")
        .with_session(sessions::MANAGER)
        .await
        .assert_json(&json!([{ "position_id": 2 }]));

    server
        .get("/v1/me/positions")
        .with_session(sessions::VIEWER)
        .await
        .assert_json(&json!([]));

    let super_in_100 = cookie_for(SessionData {
        session_id: Some(sessions::SUPER_ADMIN.into()),
        active_company_id: Some(100),
    });
    server
        .get("/v1/me/positions")
        .add_header("cookie", super_in_100)
        .await
        .assert_json(&json!([{ "position_id": 9 }]));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn positions_require_a_session(pool: MySqlPool) {
    server(pool)
        .get("/v1/me/positions")
        .await
        .assert_status_unauthorized();
}

// ---- GET/PUT /v1/me/sales-goal ----------------------------------------------

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn sales_goal_reads_and_saves(pool: MySqlPool) {
    let server = server(pool);
    let goal = |session| server.get("/v1/me/sales-goal").with_session(session);
    assert_eq!(
        goal(sessions::REP).await.json::<Value>(),
        json!({ "monthly_sales_goal": null })
    );
    assert_eq!(
        goal(sessions::MANAGER).await.json::<Value>(),
        json!({ "monthly_sales_goal": "75000.00" })
    );

    let saved: Value = server
        .put("/v1/me/sales-goal")
        .with_session(sessions::REP)
        .json(&json!({ "monthly_sales_goal": 22000.456 }))
        .await
        .json();
    assert_eq!(saved, json!({ "monthly_sales_goal": "22000.46" }));
    assert_eq!(goal(sessions::REP).await.json::<Value>(), saved);
    // Only the caller's goal changes.
    assert_eq!(
        goal(sessions::MANAGER).await.json::<Value>(),
        json!({ "monthly_sales_goal": "75000.00" })
    );

    for body in [
        json!({ "monthly_sales_goal": 0 }),
        json!({ "monthly_sales_goal": -5 }),
        json!({ "monthly_sales_goal": 0.001 }),
        json!({ "monthly_sales_goal": 10_000_000_000.0_f64 }),
        json!({ "monthly_sales_goal": "50000" }),
        json!({}),
    ] {
        server
            .put("/v1/me/sales-goal")
            .with_session(sessions::REP)
            .json(&body)
            .expect_failure()
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    assert_eq!(goal(sessions::REP).await.json::<Value>(), saved);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn sales_goal_needs_an_employee(pool: MySqlPool) {
    let server = server(pool);
    server
        .get("/v1/me/sales-goal")
        .expect_failure()
        .await
        .assert_status_unauthorized();
    server
        .put("/v1/me/sales-goal")
        .with_session(sessions::VIEWER)
        .json(&json!({ "monthly_sales_goal": 1 }))
        .expect_failure()
        .await
        .assert_status_forbidden();
}

// ---- GET /v1/me/telegram ----------------------------------------------------

/// A stand-in for api.telegram.org: `getChat` answers ok only for the chat ids
/// in `reachable`, and every call is recorded as "<token>:<chat id>".
async fn fake_telegram(reachable: &'static [&'static str]) -> (String, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&calls);
    let app = Router::new().route(
        "/{bot}/getChat",
        post(
            move |AxumPath(bot): AxumPath<String>, Json(body): Json<Value>| {
                let recorded = Arc::clone(&recorded);
                async move {
                    let chat = body["chat_id"].as_str().unwrap_or_default().to_string();
                    recorded.lock().unwrap().push(format!("{bot}:{chat}"));
                    Json(json!({ "ok": reachable.contains(&chat.as_str()) }))
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, calls)
}

async fn telegram_ids(pool: &MySqlPool, user_id: i32) -> (Option<i64>, Option<i64>) {
    sqlx::query_as("SELECT telegram_id, notifications_telegram_id FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn telegram_status_without_bots_trusts_the_saved_links(pool: MySqlPool) {
    let server = server(pool);
    let status = |session| server.get("/v1/me/telegram").with_session(session);
    assert_eq!(
        status(sessions::REP).await.json::<Value>(),
        json!({
            "leads_linked": false,
            "notifications_linked": false,
            "leads_bot_id": null,
            "notifications_bot_id": null
        })
    );
    assert_eq!(
        status(sessions::MANAGER).await.json::<Value>(),
        json!({
            "leads_linked": true,
            "notifications_linked": true,
            "leads_bot_id": null,
            "notifications_bot_id": null
        })
    );
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn telegram_links_the_bot_cannot_reach_are_removed(pool: MySqlPool) {
    // Mark's leads chat (900101) still works; his notifications chat (910101)
    // blocked the bot.
    let (base, calls) = fake_telegram(&["900101"]).await;
    let bots = TelegramBots::new(Some("111:leads"), Some(" 222:notes "), &base);
    let server = TestServer::new(app(
        AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN).with_telegram(bots)
    ));

    let status: Value = server
        .get("/v1/me/telegram")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(
        status,
        json!({
            "leads_linked": true,
            "notifications_linked": false,
            "leads_bot_id": 111,
            "notifications_bot_id": 222
        })
    );
    let mut asked = calls.lock().unwrap().clone();
    asked.sort();
    assert_eq!(asked, vec!["bot111:leads:900101", "bot222:notes:910101"]);
    assert_eq!(telegram_ids(&pool, 101).await, (Some(900_101), None));

    // Nothing linked: Telegram is not asked.
    calls.lock().unwrap().clear();
    let status: Value = server
        .get("/v1/me/telegram")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(status["leads_linked"], false);
    assert!(calls.lock().unwrap().is_empty());
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn telegram_links_stay_when_telegram_cannot_be_reached(pool: MySqlPool) {
    // Nothing listens there: the check fails, which is not a "no".
    let bots = TelegramBots::new(Some("111:leads"), Some("222:notes"), "http://127.0.0.1:9");
    let server = TestServer::new(app(
        AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN).with_telegram(bots)
    ));
    let status: Value = server
        .get("/v1/me/telegram")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(
        (&status["leads_linked"], &status["notifications_linked"]),
        (&json!(true), &json!(true))
    );
    assert_eq!(
        telegram_ids(&pool, 101).await,
        (Some(900_101), Some(910_101))
    );
}
