mod common;

use api::storage::{MEMORY_PUBLIC_BASE, Storage};
use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, png, server, server_with_storage, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: stones 3000-3010 of company 100 (3000 Absolute Black with slabs
// 6000-6003 in the yard, colors Black; 3008 Zebra Quartz with slab 6040),
// 4000 of company 101. Installed photos 100/101 on 3000, 102 on 3008, 103 on
// 4000. Stone types 100-103 (company 100), 104 (company 101). Suppliers 300,
// 301 (company 100), 302 (company 101). Only admins (ADMIN, SUPER_ADMIN)
// may write.

/// Uploads a PNG the way a browser would: presign, then PUT the bytes.
async fn upload(server: &TestServer, storage: &Storage, folder: &str) -> String {
    let res = server
        .post("/v1/uploads")
        .with_session(sessions::ADMIN)
        .json(&json!({ "folder": folder, "file_name": "photo.PNG", "content_type": "image/png" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let body: Value = res.json();
    let url = body["url"].as_str().unwrap().to_string();
    let key = storage.key_for_url(&url).unwrap();
    storage.put(&key, png(800, 600), "image/png").await.unwrap();
    url
}

fn key(url: &str) -> String {
    url.strip_prefix(&format!("{MEMORY_PUBLIC_BASE}/"))
        .unwrap()
        .to_string()
}

fn icon(url: &str) -> String {
    api::storage::icon_key(&key(url))
}

fn stone_body(image_url: Option<&str>) -> Value {
    json!({
        "name": "Blue Pearl",
        "type": "Granite",
        "finishing": "polished",
        "image_url": image_url,
        "is_display": true,
        "on_sale": false,
        "regular_stock": true,
        "supplier_id": 301,
        "length": 120.5,
        "width": 70,
        "cost_per_sqft": 45,
        "retail_price": 99,
        "level": 3,
        "bundle_number": "BP-1",
        "bundle_location": " ",
        "delivery_date": "2026-11-02",
        "color_ids": [3, 13, 3]
    })
}

async fn expect_error(res: axum_test::TestResponse, status: StatusCode, starts_with: &str) {
    res.assert_status(status);
    let message = res.json::<Value>()["error"].as_str().unwrap().to_string();
    assert!(message.starts_with(starts_with), "{message}");
}

// ---- POST /v1/uploads

#[test_db::test(seed)]
async fn uploads_are_presigned_into_the_folder(pool: MySqlPool) {
    let (server, _) = server_with_storage(pool);
    let res = server
        .post("/v1/uploads")
        .with_session(sessions::ADMIN)
        .json(&json!({ "folder": "sinks", "file_name": "a.JPEG", "content_type": "Image/JPEG" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let body: Value = res.json();
    let url = body["url"].as_str().unwrap();
    assert!(
        url.starts_with(&format!("{MEMORY_PUBLIC_BASE}/sinks/")) && url.ends_with(".jpeg"),
        "{url}"
    );
    assert!(body["upload_url"].as_str().unwrap().contains("/sinks/"));

    expect_error(
        server
            .post("/v1/uploads")
            .with_session(sessions::ADMIN)
            .json(&json!({ "folder": "stones", "file_name": "a.pdf", "content_type": "application/pdf" }))
            .await,
        StatusCode::BAD_REQUEST,
        "unsupported content type",
    )
    .await;
    server
        .post("/v1/uploads")
        .with_session(sessions::ADMIN)
        .json(&json!({ "folder": "secrets", "file_name": "a.png", "content_type": "image/png" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/uploads")
        .with_session(sessions::REP)
        .json(&json!({ "folder": "stones", "file_name": "a.png", "content_type": "image/png" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn uploads_need_configured_storage(pool: MySqlPool) {
    expect_error(
        server(pool)
            .post("/v1/uploads")
            .with_session(sessions::ADMIN)
            .json(&json!({ "folder": "stones", "file_name": "a.png", "content_type": "image/png" }))
            .await,
        StatusCode::SERVICE_UNAVAILABLE,
        "uploads are not configured",
    )
    .await;
}

// ---- Catalog

#[test_db::test(seed)]
async fn the_supplier_directory_lists_contacts_and_files(pool: MySqlPool) {
    sqlx::raw_sql(
        "UPDATE suppliers SET manager = 'Ann', phone = '317-555-0300' WHERE id = 300;
         INSERT INTO supplier_files (id, name, url, supplier_id)
           VALUES (9500, 'Prices.pdf', 'https://files.example/prices.pdf', 300),
                  (9501, 'Other.pdf', 'https://files.example/other.pdf', 302);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let directory: Value = server
        .get("/v1/suppliers/directory")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(common::ids(&directory), [300, 301]);
    assert_eq!(directory[0]["manager"], "Ann");
    assert_eq!(
        directory[0]["files"],
        json!([{ "id": 9500, "name": "Prices.pdf", "url": "https://files.example/prices.pdf" }])
    );
    assert_eq!(directory[1]["files"], json!([]));
}

#[test_db::test(seed)]
async fn suppliers_colors_and_types_of_the_company(pool: MySqlPool) {
    let server = server(pool);
    let get = |path: &'static str| server.get(path).with_session(sessions::REP);
    assert_eq!(
        get("/v1/suppliers").await.json::<Value>(),
        json!([{ "id": 301, "name": "Quartz Partners" }, { "id": 300, "name": "Stone Source Inc" }])
    );
    let colors: Value = get("/v1/colors").await.json();
    assert_eq!(colors.as_array().unwrap().len(), 16);
    assert_eq!(
        colors[0],
        json!({ "id": 1, "name": "Beige", "hex_code": "#F5F5DC" })
    );
    assert_eq!(
        get("/v1/stone-types").await.json::<Value>(),
        json!([
            { "id": 100, "name": "granite" },
            { "id": 102, "name": "marble" },
            { "id": 101, "name": "quartz" },
            { "id": 103, "name": "quartzite" }
        ])
    );
    let other: Value = server
        .get("/v1/stone-types")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(other, json!([{ "id": 104, "name": "granite" }]));
    server
        .get("/v1/suppliers")
        .await
        .assert_status_unauthorized();
}

#[test_db::test(seed)]
async fn stone_types_are_created_renamed_and_deleted(pool: MySqlPool) {
    let server = server(pool.clone());
    let created = server
        .post("/v1/stone-types")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "  Onyx " }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let id = created.json::<Value>()["id"].as_i64().unwrap();
    assert_eq!(created.json::<Value>()["name"], "Onyx");

    for (body, status, error) in [
        (
            json!({ "name": "onyx" }),
            StatusCode::CONFLICT,
            "This stone type already exists.",
        ),
        (
            json!({ "name": "" }),
            StatusCode::BAD_REQUEST,
            "Enter a stone type name",
        ),
        (
            json!({ "name": "x".repeat(256) }),
            StatusCode::BAD_REQUEST,
            "Enter a stone type name",
        ),
    ] {
        expect_error(
            server
                .post("/v1/stone-types")
                .with_session(sessions::ADMIN)
                .json(&body)
                .await,
            status,
            error,
        )
        .await;
    }
    // Company 101 may have its own "Onyx".
    server
        .post("/v1/stone-types")
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "name": "Onyx" }))
        .await
        .assert_status(StatusCode::CREATED);
    server
        .post("/v1/stone-types")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Slate" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // Renaming marble renames the stones of that type (any case).
    let res = server
        .patch("/v1/stone-types/102")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Marble Stone" }))
        .await;
    res.assert_status_ok();
    let kind: Option<String> = sqlx::query_scalar("SELECT type FROM stones WHERE id = 3002")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kind.as_deref(), Some("Marble Stone"));
    expect_error(
        server
            .patch("/v1/stone-types/102")
            .with_session(sessions::ADMIN)
            .json(&json!({ "name": "QUARTZ" }))
            .await,
        StatusCode::CONFLICT,
        "This stone type already exists.",
    )
    .await;
    server
        .patch("/v1/stone-types/104")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Mine" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // Quartz is used by Calacatta Gold and Zebra Quartz.
    let blocked = server
        .delete("/v1/stone-types/101")
        .with_session(sessions::ADMIN)
        .await;
    blocked.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        blocked.json::<Value>(),
        json!({
            "error": "This type is used by undeleted stones and cannot be deleted.",
            "stones": [{ "id": 3001, "name": "Calacatta Gold" }, { "id": 3008, "name": "Zebra Quartz" }]
        })
    );
    server
        .delete(&format!("/v1/stone-types/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete(&format!("/v1/stone-types/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

// ---- Stones

#[test_db::test(seed)]
async fn creates_a_stone_with_its_image_and_colors(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool);
    let url = upload(&server, &storage, "stones").await;
    let res = server
        .post("/v1/stones")
        .with_session(sessions::ADMIN)
        .json(&stone_body(Some(&url)))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    // The thumbnail is stored next to the image.
    assert!(storage.memory_keys().contains(&icon(&url)));

    let stone: Value = server
        .get(&format!("/v1/stones/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(stone["name"], "Blue Pearl");
    assert_eq!(stone["type"], "Granite");
    assert_eq!(stone["url"], url);
    assert_eq!(stone["supplier_id"], 301);
    assert_eq!(stone["regular_stock"], true);
    assert_eq!(stone["length"], 120.5);
    assert_eq!(stone["retail_price"], 99);
    assert_eq!(stone["level"], 3);
    assert_eq!(stone["bundle_number"], "BP-1");
    assert_eq!(stone["bundle_location"], Value::Null);
    assert_eq!(stone["delivery_date"], "2026-11-02");
    assert_eq!(stone["color_ids"], json!([3, 13]));
}

#[test_db::test(seed)]
async fn a_stone_added_without_an_image_gets_the_placeholder(pool: MySqlPool) {
    let server = server(pool.clone());
    let res = server
        .post("/v1/stones")
        .with_session(sessions::ADMIN)
        .json(&stone_body(None))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    let url: Option<String> = sqlx::query_scalar("SELECT url FROM stones WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(url.as_deref(), Some("/placeholders/stone-placeholder.svg"));
}

#[test_db::test(seed)]
async fn stone_writes_validate_everything(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    let url = upload(&server, &storage, "stones").await;
    let sink_url = upload(&server, &storage, "sinks").await;
    let with = |key: &str, value: Value| {
        let mut body = stone_body(Some(&url));
        body[key] = value;
        body
    };
    let cases = [
        (
            with("image_url", json!("https://img.example/x.jpg")),
            "not an uploaded stones file",
        ),
        (
            with("image_url", json!(sink_url)),
            "not an uploaded stones file",
        ),
        (
            with(
                "image_url",
                json!(format!("{MEMORY_PUBLIC_BASE}/stones/missing.png")),
            ),
            "file was not uploaded",
        ),
        (with("name", json!("  ")), "Name is required"),
        (with("type", json!("onyx")), "Select a valid stone type."),
        (with("finishing", json!("matte")), "invalid finishing"),
        (with("level", json!(8)), "level must be 1 to 7"),
        (with("width", json!(-1)), "Enter a number of 0 or more"),
        (
            with("delivery_date", json!("11/02/2026")),
            "invalid delivery_date",
        ),
        (with("supplier_id", json!(302)), "supplier 302 not found"),
        (with("color_ids", json!([99])), "color 99 not found"),
        (with("company_id", json!(100)), "invalid body"),
    ];
    for (body, error) in cases {
        expect_error(
            server
                .post("/v1/stones")
                .with_session(sessions::ADMIN)
                .json(&body)
                .await,
            StatusCode::BAD_REQUEST,
            error,
        )
        .await;
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stones WHERE name = 'Blue Pearl'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    server
        .post("/v1/stones")
        .with_session(sessions::REP)
        .json(&stone_body(Some(&url)))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn editing_keeps_or_replaces_the_image(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    let first = upload(&server, &storage, "stones").await;
    let id = server
        .post("/v1/stones")
        .with_session(sessions::ADMIN)
        .json(&stone_body(Some(&first)))
        .await
        .json::<Value>()["id"]
        .as_i64()
        .unwrap();

    // No image_url: kept. Colors are replaced.
    let mut body = stone_body(None);
    body["name"] = json!("Blue Pearl GT");
    body["color_ids"] = json!([2]);
    body["supplier_id"] = Value::Null;
    body["level"] = Value::Null;
    server
        .put(&format!("/v1/stones/{id}"))
        .with_session(sessions::ADMIN)
        .json(&body)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let stone: Value = server
        .get(&format!("/v1/stones/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(stone["name"], "Blue Pearl GT");
    assert_eq!(stone["url"], first);
    assert_eq!(stone["color_ids"], json!([2]));
    assert_eq!(stone["supplier_id"], Value::Null);
    assert_eq!(stone["level"], Value::Null);

    // A new image replaces the old one, whose files are deleted.
    let second = upload(&server, &storage, "stones").await;
    body["image_url"] = json!(second);
    server
        .put(&format!("/v1/stones/{id}"))
        .with_session(sessions::ADMIN)
        .json(&body)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let keys = storage.memory_keys();
    assert!(
        !keys.contains(&key(&first)) && !keys.contains(&icon(&first)),
        "{keys:?}"
    );
    assert!(
        keys.contains(&key(&second)) && keys.contains(&icon(&second)),
        "{keys:?}"
    );

    // A photo another stone also uses stays when it is replaced.
    sqlx::query("UPDATE stones SET url = ? WHERE id = 3001")
        .bind(&second)
        .execute(&pool)
        .await
        .unwrap();
    body["image_url"] = json!(upload(&server, &storage, "stones").await);
    server
        .put(&format!("/v1/stones/{id}"))
        .with_session(sessions::ADMIN)
        .json(&body)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(storage.memory_keys().contains(&key(&second)));

    // Seed stones keep images outside the bucket untouched.
    let mut seed = stone_body(None);
    seed["name"] = json!("Absolute Black");
    server
        .put("/v1/stones/3000")
        .with_session(sessions::ADMIN)
        .json(&seed)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    for id in [4000, 3005, 999_999] {
        server
            .put(&format!("/v1/stones/{id}"))
            .with_session(sessions::ADMIN)
            .json(&seed)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
}

#[test_db::test(seed)]
async fn deleting_a_stone_with_slabs_needs_confirmation(pool: MySqlPool) {
    let server = server(pool.clone());
    expect_error(
        server
            .delete("/v1/stones/3008")
            .with_session(sessions::ADMIN)
            .await,
        StatusCode::CONFLICT,
        "Confirm removing associated slabs",
    )
    .await;
    server
        .delete("/v1/stones/3008?include_slabs=true")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let (stone_gone, slab_gone): (bool, bool) = sqlx::query_as(
        "SELECT (SELECT deleted_at IS NOT NULL FROM stones WHERE id = 3008),
                (SELECT deleted_at IS NOT NULL FROM slab_inventory WHERE id = 6040)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(stone_gone && slab_gone);
    server
        .get("/v1/stones/3008")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // No slabs: one step.
    server
        .delete("/v1/stones/3003")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    for id in [4000, 3008] {
        server
            .delete(&format!("/v1/stones/{id}?include_slabs=true"))
            .with_session(sessions::ADMIN)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    server
        .delete("/v1/stones/3000")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

// ---- Installed photos

#[test_db::test(seed)]
async fn photos_are_listed_added_linked_and_deleted(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    let list = |id: i64| {
        server
            .get(&format!("/v1/stones/{id}/images"))
            .with_session(sessions::REP)
    };
    assert_eq!(
        list(3000).await.json::<Value>(),
        json!([
            { "id": 100, "url": "https://img.example/installed-black-1.jpg", "source_stone": null },
            { "id": 101, "url": "https://img.example/installed-black-2.jpg", "source_stone": null }
        ])
    );

    let a = upload(&server, &storage, "stones").await;
    let b = upload(&server, &storage, "stones").await;
    let added = server
        .post("/v1/stones/3000/images")
        .with_session(sessions::ADMIN)
        .json(&json!({ "urls": [a, b] }))
        .await;
    added.assert_status(StatusCode::CREATED);
    let added: Value = added.json();
    assert_eq!(added.as_array().unwrap().len(), 2);
    assert!(storage.memory_keys().contains(&icon(&a)));
    assert!(storage.memory_keys().contains(&icon(&b)));

    // Zebra Quartz's photo shows up after linking.
    server
        .post("/v1/stones/3000/image-links")
        .with_session(sessions::ADMIN)
        .json(&json!({ "source_stone_id": 3008 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let images: Value = list(3000).await.json();
    let last = images.as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last,
        json!({ "id": 102, "url": "https://img.example/installed-zebra-1.jpg",
                "source_stone": { "id": 3008, "name": "Zebra Quartz" } })
    );
    assert_eq!(images.as_array().unwrap().len(), 5);

    // Deleting an own photo removes its files; a linked photo is not its to delete.
    let new_id = added[0]["id"].as_i64().unwrap();
    server
        .delete(&format!("/v1/stones/3000/images/{new_id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(!storage.memory_keys().contains(&key(&a)));
    assert!(!storage.memory_keys().contains(&icon(&a)));
    server
        .delete("/v1/stones/3000/images/102")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    server
        .delete("/v1/stones/3000/image-links/3008")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        list(3000).await.json::<Value>().as_array().unwrap().len(),
        3
    );
}

#[test_db::test(seed)]
async fn photo_routes_are_scoped_and_validated(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool);
    server
        .get("/v1/stones/4000/images")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    for (body, error) in [
        (json!({ "source_stone_id": 4000 }), "stone 4000 not found"),
        (
            json!({ "source_stone_id": 3000 }),
            "a stone cannot link to itself",
        ),
    ] {
        expect_error(
            server
                .post("/v1/stones/3000/image-links")
                .with_session(sessions::ADMIN)
                .json(&body)
                .await,
            StatusCode::BAD_REQUEST,
            error,
        )
        .await;
    }
    expect_error(
        server
            .post("/v1/stones/3000/images")
            .with_session(sessions::ADMIN)
            .json(&json!({ "urls": [] }))
            .await,
        StatusCode::BAD_REQUEST,
        "send 1 to 50 urls",
    )
    .await;
    let faucet = upload(&server, &storage, "faucets").await;
    expect_error(
        server
            .post("/v1/stones/3000/images")
            .with_session(sessions::ADMIN)
            .json(&json!({ "urls": [faucet] }))
            .await,
        StatusCode::BAD_REQUEST,
        "not an uploaded stones file",
    )
    .await;
    server
        .delete("/v1/stones/4000/images/103")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn stone_names_for_linking(pool: MySqlPool) {
    let server = server(pool);
    let names = |query: &'static str| {
        server
            .get(&format!("/v1/stones/names?{query}"))
            .with_session(sessions::REP)
    };
    assert_eq!(
        names("has=images").await.json::<Value>(),
        json!([{ "id": 3000, "name": "Absolute Black" }, { "id": 3008, "name": "Zebra Quartz" }])
    );
    let slabs: Value = names("has=slabs").await.json();
    let ids: Vec<i64> = common::ids(&slabs);
    // Hidden Granite (3004) has a slab too; deleted 3005 and company 101 never.
    assert_eq!(ids, vec![3000, 3001, 3004, 3006, 3009, 3010, 3008]);
    names("has=photos")
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

// ---- Slab links and slabs

#[test_db::test(seed)]
async fn slab_links_add_the_source_slabs(pool: MySqlPool) {
    let server = server(pool);
    server
        .post("/v1/stones/3000/slab-links")
        .with_session(sessions::ADMIN)
        .json(&json!({ "source_stone_id": 3008 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let slabs: Value = server
        .get("/v1/stones/3000/slabs")
        .with_session(sessions::REP)
        .await
        .json();
    assert!(common::ids(&slabs).contains(&6040));
    let links: Value = server
        .get("/v1/stones/3000/links")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        links,
        json!({
            "slab_sources": [{ "id": 3008, "name": "Zebra Quartz" }],
            "slab_dependents": [{ "id": 3007, "name": "Linked Black Remnants" }],
            "image_sources": []
        })
    );
    // 3007 already draws from 3000: the reverse would loop.
    expect_error(
        server
            .post("/v1/stones/3000/slab-links")
            .with_session(sessions::ADMIN)
            .json(&json!({ "source_stone_id": 3007 }))
            .await,
        StatusCode::BAD_REQUEST,
        "stone 3007 already draws slabs from this stone",
    )
    .await;
    // Twice is a no-op.
    server
        .post("/v1/stones/3000/slab-links")
        .with_session(sessions::ADMIN)
        .json(&json!({ "source_stone_id": 3008 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete("/v1/stones/3000/slab-links/3008")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let slabs: Value = server
        .get("/v1/stones/3000/slabs")
        .with_session(sessions::REP)
        .await
        .json();
    assert!(!common::ids(&slabs).contains(&6040));
}

#[test_db::test(seed)]
async fn slabs_are_added_renamed_and_deleted(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    // No size: the stone's (Zebra Quartz is 118 x 56).
    let res = server
        .post("/v1/stones/3008/slabs")
        .with_session(sessions::ADMIN)
        .json(&json!({ "bundle": " Z-2 " }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let slab: Value = res.json();
    assert_eq!(slab["bundle"], "Z-2");
    assert_eq!(
        (slab["length"].as_str(), slab["width"].as_str()),
        (Some("118"), Some("56"))
    );
    assert_eq!(slab["url"], Value::Null);

    let photo = upload(&server, &storage, "stones").await;
    let with_photo: Value = server
        .post("/v1/stones/3008/slabs")
        .with_session(sessions::ADMIN)
        .json(&json!({ "bundle": "Z-3", "length": 110, "width": 55.5, "image_url": photo }))
        .await
        .json();
    assert_eq!(with_photo["length"], "110");
    assert_eq!(with_photo["width"], "55.5");
    assert_eq!(with_photo["url"], photo);
    assert!(storage.memory_keys().contains(&icon(&photo)));

    expect_error(
        server
            .post("/v1/stones/3008/slabs")
            .with_session(sessions::ADMIN)
            .json(&json!({ "bundle": "" }))
            .await,
        StatusCode::BAD_REQUEST,
        "Bundle is required",
    )
    .await;
    server
        .post("/v1/stones/4000/slabs")
        .with_session(sessions::ADMIN)
        .json(&json!({ "bundle": "X" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);

    let id = slab["id"].as_i64().unwrap();
    server
        .patch(&format!("/v1/slabs/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "bundle": "Z-2A" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let bundle: Option<String> =
        sqlx::query_scalar("SELECT bundle FROM slab_inventory WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(bundle.as_deref(), Some("Z-2A"));

    server
        .delete(&format!("/v1/slabs/{id}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .delete(&format!("/v1/slabs/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let slabs: Value = server
        .get("/v1/stones/3008/slabs")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert!(!common::ids(&slabs).contains(&id));
    server
        .delete("/v1/slabs/6060")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

// ---- slab numbers may repeat on one stone (e.g. several "On the way") --------

#[test_db::test(seed)]
async fn slab_numbers_may_repeat_on_a_stone(pool: MySqlPool) {
    let server = server(pool.clone());
    let res = server
        .post("/v1/stones")
        .with_session(sessions::ADMIN)
        .json(&stone_body(None))
        .await;
    res.assert_status(StatusCode::CREATED);
    let stone = res.json::<Value>()["id"].as_i64().unwrap();

    let add = |bundle: &str| {
        server
            .post(&format!("/v1/stones/{stone}/slabs"))
            .with_session(sessions::ADMIN)
            .json(&json!({ "bundle": bundle, "length": 120, "width": 60 }))
    };
    add("On the way").await.assert_status(StatusCode::CREATED);
    add("On the way").await.assert_status(StatusCode::CREATED);
    let second = add("QA-DUP-2").await;
    second.assert_status(StatusCode::CREATED);
    let second = second.json::<Value>()["id"].as_i64().unwrap();

    // Renaming onto a number another slab of this stone has is fine too.
    server
        .patch(&format!("/v1/slabs/{second}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "bundle": "On the way" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
}
