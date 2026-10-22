mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn shop_workers_see_installed_photos(pool: MySqlPool) {
    let server = server(pool.clone());
    // Vic (104) is not an employee.
    server
        .get("/v1/stones/3000/images")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (104, 4, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let res = server
        .get("/v1/stones/3000/images")
        .with_session(sessions::VIEWER)
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()[0]["id"], 100);
    server
        .get("/v1/sinks/1/images")
        .with_session(sessions::VIEWER)
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn an_emails_pictures_stay_in_its_company(pool: MySqlPool) {
    let server = server(pool);
    let own: Value = server
        .get("/v1/emails/8802/images")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(
        own,
        json!([{ "id": 8810, "url": "https://example.test/sketch.png" }])
    );
    let other: Value = server
        .get("/v1/emails/8802/images")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(other, json!([]));
}
