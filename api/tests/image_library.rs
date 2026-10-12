mod common;

use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, png, server_with_storage, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const ADMIN: &str = sessions::ADMIN;

async fn upload(server: &TestServer, storage: &api::storage::Storage) -> String {
    let res = server
        .post("/v1/uploads")
        .with_session(ADMIN)
        .json(&json!({ "folder": "images", "file_name": "p.png", "content_type": "image/png" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let url = res.json::<Value>()["url"].as_str().unwrap().to_string();
    storage
        .put(
            &storage.key_for_url(&url).unwrap(),
            png(400, 300),
            "image/png",
        )
        .await
        .unwrap();
    url
}

async fn library(server: &TestServer, session: &str) -> Value {
    let res = server.get("/v1/image-library").with_session(session).await;
    res.assert_status_ok();
    res.json()
}

fn names(images: &Value) -> Vec<&str> {
    images
        .as_array()
        .unwrap()
        .iter()
        .map(|image| image["name"].as_str().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn the_library_groups_images_by_folder(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO images_folders (id, name, url, company_id, created_by, deleted_at) VALUES
           (9700, 'kitchens', '', 100, 'Ada', NULL),
           (9701, 'Baths', '', 100, 'Ada', NULL),
           (9702, 'Old', '', 100, 'Ada', '2026-01-01 00:00:00'),
           (9703, 'Theirs', '', 101, 'Olga', NULL);
         INSERT INTO images (id, name, url, company_id, folder_id) VALUES
           (9800, 'b', 'https://i/b.jpg', 100, NULL),
           (9801, 'A', 'https://i/a.jpg', 100, NULL),
           (9802, 'Island', 'https://i/i.jpg', 100, 9700),
           (9803, 'Gone', 'https://i/g.jpg', 100, 9702),
           (9804, 'Other', 'https://i/o.jpg', 101, NULL);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (server, _) = server_with_storage(pool);
    let body = library(&server, sessions::REP).await;
    assert_eq!(names(&body["root_images"]), ["A", "b"]);
    let folders: Vec<(&str, Vec<&str>)> = body["folders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|folder| (folder["name"].as_str().unwrap(), names(&folder["images"])))
        .collect();
    assert_eq!(folders, [("Baths", vec![]), ("kitchens", vec!["Island"])]);
    assert_eq!(body["folders"][1]["images"][0]["folder_id"], 9700);
}

#[test_db::test(seed)]
async fn admins_keep_images_and_folders(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    let res = server
        .post("/v1/image-folders")
        .with_session(ADMIN)
        .json(&json!({ "name": "Kitchens" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let folder = res.json::<Value>()["id"].as_i64().unwrap();

    let url = upload(&server, &storage).await;
    let res = server
        .post("/v1/images")
        .with_session(ADMIN)
        .json(&json!({ "name": "Island", "url": url, "folder_id": folder }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let image: Value = res.json();
    assert_eq!(image["folder_id"], folder);
    let id = image["id"].as_i64().unwrap();
    // Its thumbnail was made.
    let icon = format!("{}-icon.png", url.trim_end_matches(".png"));
    assert!(
        storage
            .memory_keys()
            .contains(&storage.key_for_url(&icon).unwrap())
    );
    let created_by: Option<String> =
        sqlx::query_scalar("SELECT created_by FROM images WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(created_by.as_deref(), Some("Ada Admin"));

    // A new file replaces the old one and its thumbnail.
    let replacement = upload(&server, &storage).await;
    let res = server
        .patch(&format!("/v1/images/{id}"))
        .with_session(ADMIN)
        .json(&json!({ "name": "Island 2", "url": replacement }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["name"], "Island 2");
    let keys = storage.memory_keys();
    assert!(!keys.contains(&storage.key_for_url(&url).unwrap()));
    assert!(!keys.contains(&storage.key_for_url(&icon).unwrap()));

    // Moved to another folder, which is then renamed and deleted.
    let other = server
        .post("/v1/image-folders")
        .with_session(ADMIN)
        .json(&json!({ "name": "Baths" }))
        .await
        .json::<Value>()["id"]
        .as_i64()
        .unwrap();
    let res = server
        .patch(&format!("/v1/images/{id}"))
        .with_session(ADMIN)
        .json(&json!({ "folder_id": other }))
        .await;
    assert_eq!(res.json::<Value>()["folder_id"], other);
    server
        .patch(&format!("/v1/image-folders/{other}"))
        .with_session(ADMIN)
        .json(&json!({ "name": "Bathrooms" }))
        .await
        .assert_status_ok();
    let body = library(&server, ADMIN).await;
    assert_eq!(body["folders"][0]["name"], "Bathrooms");
    assert_eq!(names(&body["folders"][0]["images"]), ["Island 2"]);
    server
        .delete(&format!("/v1/image-folders/{other}"))
        .with_session(ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .patch(&format!("/v1/images/{id}"))
        .with_session(ADMIN)
        .json(&json!({ "folder_id": other }))
        .await
        .assert_status(StatusCode::NOT_FOUND);

    server
        .delete(&format!("/v1/images/{id}"))
        .with_session(ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete(&format!("/v1/images/{id}"))
        .with_session(ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn image_writes_are_for_admins_of_the_company(pool: MySqlPool) {
    sqlx::query("INSERT INTO images (id, name, url, company_id) VALUES (9804, 'Other', 'x', 101)")
        .execute(&pool)
        .await
        .unwrap();
    let (server, storage) = server_with_storage(pool);
    let url = upload(&server, &storage).await;
    server
        .post("/v1/images")
        .with_session(sessions::REP)
        .json(&json!({ "name": "X", "url": url }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/images")
        .with_session(ADMIN)
        .json(&json!({ "name": "X", "url": "https://elsewhere.test/x.png" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .patch("/v1/images/9804")
        .with_session(ADMIN)
        .json(&json!({ "name": "Mine" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .post("/v1/image-folders")
        .with_session(sessions::REP)
        .json(&json!({ "name": "X" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}
