use crate::common;

use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, server_with_storage, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn upload(server: &TestServer, storage: &api::storage::Storage, file: &str) -> String {
    let res = server
        .post("/v1/uploads")
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "folder": "documents", "file_name": file, "content_type": "application/pdf" }),
        )
        .await;
    res.assert_status(StatusCode::CREATED);
    let url = res.json::<Value>()["url"].as_str().unwrap().to_string();
    let key = storage.key_for_url(&url).unwrap();
    storage
        .put(&key, b"%PDF-1.4".to_vec(), "application/pdf")
        .await
        .unwrap();
    url
}

async fn names(server: &TestServer, session: &str) -> Vec<String> {
    let rows: Value = server
        .get("/v1/documents")
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
async fn admins_keep_the_document_library(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool);
    let url = upload(&server, &storage, "warranty.pdf").await;
    let res = server
        .post("/v1/documents")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": " Warranty ", "url": url }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let created: Value = res.json();
    assert_eq!(created["name"], "Warranty");
    let id = created["id"].as_i64().unwrap();
    // Employees read the library, ordered by name.
    server
        .post("/v1/documents")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Care guide", "url": upload(&server, &storage, "care.pdf").await }))
        .await
        .assert_status(StatusCode::CREATED);
    assert_eq!(
        names(&server, sessions::REP).await,
        ["Care guide", "Warranty"]
    );
    assert!(names(&server, sessions::OTHER_COMPANY_REP).await.is_empty());

    // A new file replaces the old one, which leaves storage with its preview.
    let preview = format!("{}-preview.webp", url.trim_end_matches(".pdf"));
    storage
        .put(
            &storage.key_for_url(&preview).unwrap(),
            vec![1],
            "image/webp",
        )
        .await
        .unwrap();
    let replacement = upload(&server, &storage, "warranty-2.pdf").await;
    let res = server
        .put(&format!("/v1/documents/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Warranty 2026", "url": replacement }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["url"], replacement);
    let keys = storage.memory_keys();
    assert!(!keys.contains(&storage.key_for_url(&url).unwrap()));
    assert!(!keys.contains(&storage.key_for_url(&preview).unwrap()));
    // Renaming alone keeps the file.
    let res = server
        .put(&format!("/v1/documents/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Warranty" }))
        .await;
    assert_eq!(res.json::<Value>()["url"], replacement);

    server
        .delete(&format!("/v1/documents/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(names(&server, sessions::REP).await, ["Care guide"]);
}

#[test_db::test(seed)]
async fn document_writes_are_checked(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    let url = upload(&server, &storage, "a.pdf").await;
    for (session, body, status) in [
        (
            sessions::REP,
            json!({ "name": "A", "url": url }),
            StatusCode::FORBIDDEN,
        ),
        (
            sessions::ADMIN,
            json!({ "name": " ", "url": url }),
            StatusCode::BAD_REQUEST,
        ),
        (
            sessions::ADMIN,
            json!({ "name": "A", "url": "https://elsewhere.test/a.pdf" }),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        server
            .post("/v1/documents")
            .with_session(session)
            .json(&body)
            .await
            .assert_status(status);
    }
    server
        .post("/v1/uploads")
        .with_session(sessions::REP)
        .json(&json!({ "folder": "documents", "file_name": "a.pdf", "content_type": "application/pdf" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // Another company's document is not there.
    sqlx::query(
        "INSERT INTO documents (id, name, url, company_id) VALUES (9900, 'Theirs', 'x', 101)",
    )
    .execute(&pool)
    .await
    .unwrap();
    server
        .put("/v1/documents/9900")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Mine" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete("/v1/documents/9900")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
