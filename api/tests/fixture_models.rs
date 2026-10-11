mod common;

use api::storage::Storage;
use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, ids, png, server, server_with_storage, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: sink model 3100 Undermount 3219 (units 6100, 6101 free, 6102 on slab
// 6003, 6103 deleted; photo 100), 3106 company 101's (photo 101); faucet
// model 3200 Pull-Down Chrome (6200, 6201 free, 6202 on a slab; photo 100).

async fn upload(server: &TestServer, storage: &Storage, folder: &str) -> String {
    let body: Value = server
        .post("/v1/uploads")
        .with_session(sessions::ADMIN)
        .json(&json!({ "folder": folder, "file_name": "a.png", "content_type": "image/png" }))
        .await
        .json();
    let url = body["url"].as_str().unwrap().to_string();
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

fn sink(units: Option<i64>, image_url: Option<&str>) -> Value {
    json!({
        "name": "Undermount 3219",
        "type": "stainless 18 gauge",
        "image_url": image_url,
        "is_display": true,
        "regular_stock": false,
        "supplier_id": 300,
        "length": 32,
        "width": 19,
        "retail_price": 199,
        "cost": 90,
        "units": units
    })
}

async fn units(pool: &MySqlPool, model: i64) -> (i64, i64) {
    sqlx::query_as(
        "SELECT COUNT(*), CAST(COALESCE(SUM(slab_id IS NULL), 0) AS SIGNED) FROM sinks
         WHERE sink_type_id = ? AND is_deleted = 0",
    )
    .bind(model)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn sink_model_has_every_field_and_unit_counts(pool: MySqlPool) {
    let res = server(pool)
        .get("/v1/sinks/3100")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({
            "id": 3100, "name": "Undermount 3219", "type": "stainless 18 gauge",
            "url": "https://img.example/undermount.jpg", "is_display": true,
            "regular_stock": false, "supplier_id": 300, "length": 32, "width": 19,
            "retail_price": "199.00", "cost": "90.00", "units": 3, "installed_units": 1
        })
    );
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn faucet_model_has_no_size(pool: MySqlPool) {
    let faucet: Value = server(pool)
        .get("/v1/faucets/3200")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(faucet["length"], Value::Null);
    assert_eq!(faucet["units"], 3);
    assert_eq!(faucet["installed_units"], 1);
    assert_eq!(faucet["retail_price"], "129.99");
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn models_are_scoped_to_the_company(pool: MySqlPool) {
    let server = server(pool);
    for path in [
        "/v1/sinks/3106",
        "/v1/sinks/3104",
        "/v1/faucets/3205",
        "/v1/sinks/3106/images",
    ] {
        server
            .get(path)
            .with_session(sessions::REP)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    server
        .put("/v1/sinks/3106")
        .with_session(sessions::ADMIN)
        .json(&sink(None, None))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete("/v1/faucets/3205")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn editing_units_adds_and_removes_only_free_ones(pool: MySqlPool) {
    let server = server(pool.clone());
    let put = |body: Value| {
        server
            .put("/v1/sinks/3100")
            .with_session(sessions::ADMIN)
            .json(&body)
    };
    put(sink(Some(5), None))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(units(&pool, 3100).await, (5, 4));
    put(sink(Some(1), None))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // Only the installed unit is left.
    assert_eq!(units(&pool, 3100).await, (1, 0));
    let installed: (i32, Option<i64>) =
        sqlx::query_as("SELECT is_deleted, CAST(slab_id AS SIGNED) FROM sinks WHERE id = 6102")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(installed, (0, Some(6003)));
    let res = put(sink(Some(0), None)).await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert!(
        res.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .starts_with("1 units are installed")
    );
    // No units field: stock untouched, fields saved.
    let mut body = sink(None, None);
    body["name"] = json!("Undermount Pro");
    body["retail_price"] = json!(205.5);
    put(body).await.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(units(&pool, 3100).await, (1, 0));
    let model: Value = server
        .get("/v1/sinks/3100")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(model["name"], "Undermount Pro");
    assert_eq!(model["retail_price"], "205.50");
    assert_eq!(model["url"], "https://img.example/undermount.jpg");
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn creates_models_with_image_and_units(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    let url = upload(&server, &storage, "sinks").await;
    let mut body = sink(Some(2), Some(&url));
    body["name"] = json!("Workstation 30");
    let res = server
        .post("/v1/sinks")
        .with_session(sessions::ADMIN)
        .json(&body)
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    assert_eq!(units(&pool, id).await, (2, 2));
    assert!(
        storage
            .memory_keys()
            .iter()
            .any(|key| key.ends_with("-icon.png"))
    );
    let list: Value = server
        .get("/v1/sinks")
        .with_session(sessions::REP)
        .await
        .json();
    assert!(ids(&list).contains(&id));

    let faucet_url = upload(&server, &storage, "faucets").await;
    let res = server
        .post("/v1/faucets")
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "name": "Touchless", "type": "single handle", "image_url": faucet_url,
                       "is_display": true, "retail_price": 249 }),
        )
        .await;
    res.assert_status(StatusCode::CREATED);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn model_writes_validate(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool);
    let url = upload(&server, &storage, "sinks").await;
    let stone_url = upload(&server, &storage, "stones").await;
    let with = |key: &str, value: Value| {
        let mut body = sink(None, Some(&url));
        body[key] = value;
        body
    };
    for (body, error) in [
        (sink(None, None), "An image is required"),
        (sink(None, Some(&stone_url)), "not an uploaded sinks file"),
        (with("name", json!(" ")), "Name is required"),
        (with("type", json!("bar sink")), "invalid type"),
        (with("supplier_id", json!(302)), "supplier 302 not found"),
        (
            with("cost", json!(-1)),
            "cost must be a non-negative number",
        ),
        (
            with("width", json!(-2)),
            "Size must be a non-negative number",
        ),
        (with("units", json!(5000)), "units must be 0 to 1000"),
    ] {
        let res = server
            .post("/v1/sinks")
            .with_session(sessions::ADMIN)
            .json(&body)
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        let message = res.json::<Value>()["error"].as_str().unwrap().to_string();
        assert!(message.starts_with(error), "{body}: {message}");
    }
    server
        .post("/v1/sinks")
        .with_session(sessions::REP)
        .json(&sink(None, Some(&url)))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let res = server
        .post("/v1/faucets")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "X", "type": "composite", "image_url": url }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn deleting_a_model_keeps_installed_units(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .delete("/v1/sinks/3100")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let deleted: Vec<(i32, i32)> =
        sqlx::query_as("SELECT id, is_deleted FROM sinks WHERE sink_type_id = 3100 ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(deleted, vec![(6100, 1), (6101, 1), (6102, 0), (6103, 1)]);
    server
        .get("/v1/sinks/3100")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let list: Value = server
        .get("/v1/sinks?include_sold_out=true")
        .with_session(sessions::REP)
        .await
        .json();
    assert!(!ids(&list).contains(&3100));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn model_photos(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool);
    let list = || {
        server
            .get("/v1/sinks/3100/images")
            .with_session(sessions::REP)
    };
    assert_eq!(
        list().await.json::<Value>(),
        json!([{ "id": 100, "url": "https://img.example/installed-sink-1.jpg" }])
    );
    let url = upload(&server, &storage, "sinks").await;
    let added: Value = server
        .post("/v1/sinks/3100/images")
        .with_session(sessions::ADMIN)
        .json(&json!({ "urls": [url] }))
        .await
        .json();
    let new_id = added[0]["id"].as_i64().unwrap();
    assert_eq!(list().await.json::<Value>().as_array().unwrap().len(), 2);
    server
        .delete(&format!("/v1/sinks/3100/images/{new_id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(storage.memory_keys().is_empty());
    server
        .delete("/v1/sinks/3100/images/101")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let faucet: Value = server
        .get("/v1/faucets/3200/images")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        faucet,
        json!([{ "id": 100, "url": "https://img.example/installed-faucet-1.jpg" }])
    );
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn hidden_stones_are_for_admins(pool: MySqlPool) {
    let server = server(pool);
    let admin: Value = server
        .get("/v1/stones?include_hidden=true")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let ids = ids(&admin);
    assert!(ids.contains(&3004) && ids.contains(&3010), "{ids:?}");
    server
        .get("/v1/stones?include_hidden=true")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}
