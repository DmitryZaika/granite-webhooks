use crate::common;

use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, png, server_with_storage, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn upload(server: &TestServer, storage: &api::storage::Storage) -> String {
    let res = server
        .post("/v1/uploads")
        .with_session(sessions::ADMIN)
        .json(&json!({ "folder": "supports", "file_name": "s.png", "content_type": "image/png" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let url = res.json::<Value>()["url"].as_str().unwrap().to_string();
    storage
        .put(
            &storage.key_for_url(&url).unwrap(),
            png(200, 100),
            "image/png",
        )
        .await
        .unwrap();
    url
}

async fn names(server: &TestServer, session: &str) -> Vec<String> {
    let rows: Value = server
        .get("/v1/supports")
        .with_session(session)
        .await
        .json();
    rows.as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect()
}

#[test_db::test(seed)]
async fn admins_keep_the_supports(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO supports (id, name, url, company_id) VALUES (9600, 'Theirs', 'x', 101)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (server, storage) = server_with_storage(pool);
    let url = upload(&server, &storage).await;
    let res = server
        .post("/v1/supports")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Steel bracket", "url": url }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    let icon = format!("{}-icon.png", url.trim_end_matches(".png"));
    assert!(
        storage
            .memory_keys()
            .contains(&storage.key_for_url(&icon).unwrap())
    );
    server
        .post("/v1/supports")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Corbel", "url": upload(&server, &storage).await }))
        .await
        .assert_status(StatusCode::CREATED);
    assert_eq!(
        names(&server, sessions::REP).await,
        ["Corbel", "Steel bracket"]
    );

    let replacement = upload(&server, &storage).await;
    let res = server
        .put(&format!("/v1/supports/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Hidden bracket", "url": replacement }))
        .await;
    res.assert_status_ok();
    assert!(
        !storage
            .memory_keys()
            .contains(&storage.key_for_url(&url).unwrap())
    );

    for (method, path) in [
        ("put", "/v1/supports/9600"),
        ("delete", "/v1/supports/9600"),
    ] {
        let req = if method == "put" {
            server.put(path).json(&json!({ "name": "Mine" }))
        } else {
            server.delete(path)
        };
        req.with_session(sessions::ADMIN)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    server
        .post("/v1/supports")
        .with_session(sessions::REP)
        .json(&json!({ "name": "X", "url": replacement }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .delete(&format!("/v1/supports/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(names(&server, sessions::REP).await, ["Corbel"]);
}
