use crate::common;

use axum::http::StatusCode;
use common::{WithSession, png, server_with_storage, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn upload(server: &axum_test::TestServer, storage: &api::storage::Storage) -> String {
    let upload: Value = server
        .post("/v1/uploads")
        .with_session(sessions::REP)
        .json(
            &json!({ "folder": "user-photos", "file_name": "me.jpg", "content_type": "image/png" }),
        )
        .await
        .json();
    let url = upload["url"].as_str().unwrap().to_string();
    storage
        .put(
            &storage.key_for_url(&url).unwrap(),
            png(64, 64),
            "image/png",
        )
        .await
        .unwrap();
    url
}

#[test_db::test(seed)]
async fn a_rep_sets_changes_and_removes_their_picture(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    let account: Value = server
        .get("/v1/me/account")
        .with_session(sessions::REP)
        .await
        .json();
    assert!(account["photo_url"].is_null());

    let first = upload(&server, &storage).await;
    let saved: Value = server
        .put("/v1/me/photo")
        .with_session(sessions::REP)
        .json(&json!({ "url": first }))
        .await
        .json();
    assert_eq!(saved["photo_url"], first);
    let account: Value = server
        .get("/v1/me/account")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(account["photo_url"], first);

    // A new one replaces it, and the old file goes.
    let second = upload(&server, &storage).await;
    server
        .put("/v1/me/photo")
        .with_session(sessions::REP)
        .json(&json!({ "url": second }))
        .await
        .assert_status_ok();
    let first_key = storage.key_for_url(&first).unwrap();
    assert!(storage.get(&first_key).await.unwrap().is_none());

    server
        .delete("/v1/me/photo")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
    let stored: Option<String> = sqlx::query_scalar("SELECT photo_url FROM users WHERE id = 100")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(stored.is_none());
    let second_key = storage.key_for_url(&second).unwrap();
    assert!(storage.get(&second_key).await.unwrap().is_none());
}

#[test_db::test(seed)]
async fn only_an_uploaded_picture_is_taken(pool: MySqlPool) {
    let (server, _storage) = server_with_storage(pool.clone());
    server
        .put("/v1/me/photo")
        .with_session(sessions::REP)
        .json(&json!({ "url": "https://example.com/someone-else.jpg" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let stored: Option<String> = sqlx::query_scalar("SELECT photo_url FROM users WHERE id = 100")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(stored.is_none());
}
