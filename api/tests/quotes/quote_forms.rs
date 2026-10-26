//! Printed quote and contract forms (Admin > Quotes > Forms).

use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, server_with_storage, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn create(server: &axum_test::TestServer, name: &str, kind: &str) -> i64 {
    let res = server
        .post("/v1/quote-forms")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": name, "config": { "kind": kind, "header": {}, "rows": [] } }))
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json::<Value>()["id"].as_i64().unwrap()
}

#[test_db::test(seed)]
async fn admins_manage_forms_and_employees_read_them(pool: MySqlPool) {
    let server = server(pool.clone());
    let quote = create(&server, "Standard quote", "quote").await;
    let contract = create(&server, "Kitchen contract", "contract").await;
    server
        .post("/v1/quote-forms")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "  ", "config": {} }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let res = server
        .post("/v1/quote-forms")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Bad", "config": { "kind": "invoice" } }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "The form could not be read.");

    let res = server
        .post(&format!("/v1/quote-forms/{quote}/copies"))
        .with_session(sessions::ADMIN)
        .await;
    res.assert_status(StatusCode::CREATED);
    let copy = res.json::<Value>()["id"].as_i64().unwrap();
    server
        .patch(&format!("/v1/quote-forms/{copy}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "status": "inactive" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let forms = server
        .get("/v1/quote-forms")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(
        forms,
        json!([
            { "id": quote, "name": "Standard quote", "status": "active", "sequence": 1, "kind": "quote" },
            { "id": contract, "name": "Kitchen contract", "status": "active", "sequence": 2, "kind": "contract" },
            { "id": copy, "name": "Standard quote (copy)", "status": "inactive", "sequence": 3, "kind": "quote" },
        ])
    );
    let active = server
        .get("/v1/quote-forms?active=true")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(active.as_array().unwrap().len(), 2);

    server
        .put("/v1/quote-forms/order")
        .with_session(sessions::ADMIN)
        .json(&json!({ "ids": [contract, quote, contract, 999_999] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let forms = server
        .get("/v1/quote-forms")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(forms[0]["id"], contract);

    server
        .delete(&format!("/v1/quote-forms/{copy}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get(&format!("/v1/quote-forms/{copy}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // Reps read forms but cannot change them.
    server
        .patch(&format!("/v1/quote-forms/{quote}"))
        .with_session(sessions::REP)
        .json(&json!({ "status": "inactive" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn the_editor_saves_the_layout(pool: MySqlPool) {
    let server = server(pool.clone());
    let id = create(&server, "Standard quote", "quote").await;
    let config = json!({ "kind": "quote", "header": { "title": "Quote" }, "rows": [{ "fields": [{ "id": "f-1", "type": "text", "text": "Thank you" }] }] });
    server
        .put(&format!("/v1/quote-forms/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": " Retail quote ", "status": "active", "sequence": 4.4, "config": config }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let form = server
        .get(&format!("/v1/quote-forms/{id}"))
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(form["name"], "Retail quote");
    assert_eq!(form["sequence"], 4);
    assert_eq!(form["config"], config);

    let long =
        json!({ "kind": "quote", "rows": [{ "fields": [{ "text": "x".repeat(200_001) }] }] });
    let res = server
        .put(&format!("/v1/quote-forms/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Retail quote", "status": "active", "sequence": 1, "config": long }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        res.json::<Value>()["error"],
        "A text block is too long. The limit is 200,000 characters."
    );

    let letterhead = server
        .get("/v1/quote-forms/letterhead")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(letterhead["name"], "Acme Stone");
    assert!(letterhead.get("logoUrl").is_some());
}

#[test_db::test(seed)]
async fn header_logos_lose_their_blank_border(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    let res = server
        .post("/v1/uploads")
        .with_session(sessions::ADMIN)
        .json(&json!({ "folder": "quote-forms", "file_name": "logo.png", "content_type": "image/png" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let url = res.json::<Value>()["url"].as_str().unwrap().to_string();
    let key = storage.key_for_url(&url).unwrap();
    // A 40x30 white canvas around a dark 10x10 square.
    let mut canvas = image::RgbaImage::from_pixel(40, 30, image::Rgba([255, 255, 255, 255]));
    for x in 10..20 {
        for y in 5..15 {
            canvas.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
        }
    }
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(canvas)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    storage
        .put(&key, png.into_inner(), "image/png")
        .await
        .unwrap();

    let res = server
        .post("/v1/quote-forms/images")
        .with_session(sessions::ADMIN)
        .json(&json!({ "url": url, "trim_border": true }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["url"], url);
    let stored = storage.get(&key).await.unwrap().unwrap();
    let logo = image::load_from_memory(&stored.bytes).unwrap();
    assert_eq!((logo.width(), logo.height()), (10, 10));

    server
        .post("/v1/quote-forms/images")
        .with_session(sessions::ADMIN)
        .json(&json!({ "url": "https://elsewhere.test/logo.png" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}
