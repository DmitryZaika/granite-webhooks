mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::json;
use sqlx::MySqlPool;

async fn chain(pool: &MySqlPool, parent: Option<i32>) -> Vec<(i32, Option<i32>)> {
    sqlx::query_as("SELECT id, after_id FROM instructions WHERE parent_id <=> ? AND company_id = 100 ORDER BY id")
        .bind(parent)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn admins_reorder_siblings(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO instructions (id, title, parent_id, after_id, company_id) VALUES
           (500, 'A', NULL, NULL, 100), (501, 'B', NULL, 500, 100), (502, 'C', NULL, 501, 100),
           (503, 'AI follow-up instruction', NULL, NULL, 100),
           (510, 'A1', 500, NULL, 100), (520, 'Other', NULL, NULL, 101)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());
    let put = |session: &'static str, body: serde_json::Value| {
        let request = server
            .put("/v1/instructions/order")
            .with_session(session)
            .json(&body);
        async move { request.await }
    };
    put(
        sessions::REP,
        json!({ "parent_id": null, "ordered_ids": [502, 500, 501] }),
    )
    .await
    .assert_status(StatusCode::FORBIDDEN);
    put(
        sessions::ADMIN,
        json!({ "parent_id": null, "ordered_ids": [502, 500, 501] }),
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        chain(&pool, None).await,
        [(500, Some(502)), (501, Some(500)), (502, None), (503, None)]
    );
    for bad in [
        json!({ "parent_id": null, "ordered_ids": [] }),
        json!({ "parent_id": null, "ordered_ids": [500, 501] }),
        json!({ "parent_id": null, "ordered_ids": [500, 500, 501, 502] }),
        json!({ "parent_id": null, "ordered_ids": [500, 501, 502, 520] }),
        json!({ "parent_id": 500, "ordered_ids": [500] }),
    ] {
        put(sessions::ADMIN, bad)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    put(
        sessions::ADMIN,
        json!({ "parent_id": 500, "ordered_ids": [510] }),
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);
}
