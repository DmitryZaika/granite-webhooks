mod common;

use axum::http::StatusCode;
use common::{WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// A quote of company 100 and one of company 101.
async fn seed_quotes(pool: &MySqlPool) {
    sqlx::query(
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep,
                             company_id, salesperson_id, created_by, status)
         VALUES (9101, 1000, 'Kitchen', '', '2026-01-02 10:00:00', 'Rep', 100, 100, 100, 'active'),
                (9102, 1000, 'Other', '', '2026-01-03 10:00:00', 'Rep', 101, NULL, 106, 'draft')",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn quote_files_attach_list_delete(pool: MySqlPool) {
    seed_quotes(&pool).await;
    let (app, storage) = common::server_with_storage(pool.clone());
    let mut urls = Vec::new();
    for (name, content_type, bytes) in [
        ("plan.png", "image/png", common::png(20, 10)),
        (
            "Moraware quote 4222.pdf",
            "application/pdf",
            b"%PDF-1.4".to_vec(),
        ),
    ] {
        let res = app
            .post("/v1/uploads")
            .with_session(sessions::REP)
            .json(&json!({ "folder": "quote-files", "file_name": name, "content_type": content_type }))
            .await;
        res.assert_status(StatusCode::CREATED);
        let url = res.json::<Value>()["url"].as_str().unwrap().to_string();
        let key = storage.key_for_url(&url).unwrap();
        storage.put(&key, bytes, content_type).await.unwrap();
        urls.push((url, name));
    }
    app.post("/v1/quotes/9101/files")
        .with_session(sessions::REP)
        .json(&json!({ "files": urls.iter().map(|(url, name)| json!({ "url": url, "name": name })).collect::<Vec<_>>() }))
        .await
        .assert_status(StatusCode::CREATED);

    let files: Value = app
        .get("/v1/quotes/9101/files")
        .with_session(sessions::REP)
        .await
        .json();
    let names: Vec<&str> = files
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Moraware quote 4222.pdf", "plan.png"]);
    assert_eq!(files[0]["url"], urls[1].0.as_str());
    let pdf_id = files[0]["id"].as_i64().unwrap();

    // Another company's quote, and this file under another quote.
    app.get("/v1/quotes/9102/files")
        .with_session(sessions::REP)
        .await
        .assert_status_not_found();
    app.delete(&format!("/v1/quotes/9102/files/{pdf_id}"))
        .with_session(sessions::REP)
        .await
        .assert_status_not_found();

    app.delete(&format!("/v1/quotes/9101/files/{pdf_id}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(!storage.memory_keys().iter().any(|k| k.ends_with(".pdf")));
    let files: Value = app
        .get("/v1/quotes/9101/files")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(files.as_array().unwrap().len(), 1);

    // A URL that was never uploaded to the folder, and a blank name.
    app.post("/v1/quotes/9101/files")
        .with_session(sessions::REP)
        .json(&json!({ "files": [{ "url": "https://evil.test/x.pdf", "name": "x.pdf" }] }))
        .await
        .assert_status_bad_request();
    app.post("/v1/quotes/9101/files")
        .with_session(sessions::REP)
        .json(&json!({ "files": [{ "url": urls[0].0, "name": " " }] }))
        .await
        .assert_status_bad_request();
}
