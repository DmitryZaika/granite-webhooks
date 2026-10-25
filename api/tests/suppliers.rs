use crate::common;

use api::app;
use api::state::AppState;
use api::storage::Storage;
use api::supplier_file_processor::{
    ClaimedFile, Downloaded, Extractor, MAX_ATTEMPTS, PIPELINE_VERSION, claim_next, reclaim_stale,
    run_once, split_pdf,
};
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, server, server_with_storage, sessions};
use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ---- Suppliers

#[test_db::test(seed)]
async fn admins_add_edit_and_delete_suppliers(pool: MySqlPool) {
    let server = server(pool.clone());
    let res = server
        .post("/v1/suppliers")
        .with_session(sessions::ADMIN)
        .json(&json!({
            "supplier_name": "  Granite World ",
            "website": "https://granite.example",
            "manager": "Gina",
            "phone": "317-555-0100",
            "email": "",
            "notes": ""
        }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();

    let supplier: Value = server
        .get(&format!("/v1/suppliers/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(
        supplier,
        json!({
            "id": id,
            "supplier_name": "Granite World",
            "website": "https://granite.example",
            "manager": "Gina",
            "phone": "317-555-0100",
            "email": null,
            "notes": null
        })
    );

    server
        .put(&format!("/v1/suppliers/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "supplier_name": "Granite World 2", "email": "sales@granite.example" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let supplier: Value = server
        .get(&format!("/v1/suppliers/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(supplier["supplier_name"], "Granite World 2");
    assert_eq!(supplier["email"], "sales@granite.example");
    assert_eq!(supplier["website"], Value::Null);

    // The directory lists it for every employee.
    let directory: Value = server
        .get("/v1/suppliers/directory")
        .with_session(sessions::REP)
        .await
        .json();
    assert!(
        directory
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["supplier_name"] == "Granite World 2")
    );

    server
        .delete(&format!("/v1/suppliers/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get(&format!("/v1/suppliers/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn supplier_input_is_checked(pool: MySqlPool) {
    let server = server(pool);
    for body in [
        json!({ "supplier_name": "   " }),
        json!({ "supplier_name": "A", "website": "not a url" }),
        json!({ "supplier_name": "A", "phone": "12345" }),
        json!({ "supplier_name": "A", "email": "nope" }),
        json!({ "supplier_name": "A", "colour": "red" }),
    ] {
        server
            .post("/v1/suppliers")
            .with_session(sessions::ADMIN)
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
}

#[test_db::test(seed)]
async fn suppliers_are_admin_only_and_scoped_to_the_company(pool: MySqlPool) {
    let server = server(pool);
    server
        .post("/v1/suppliers")
        .json(&json!({ "supplier_name": "A" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post("/v1/suppliers")
        .with_session(sessions::REP)
        .json(&json!({ "supplier_name": "A" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // 302 belongs to company 101.
    server
        .get("/v1/suppliers/302")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put("/v1/suppliers/302")
        .with_session(sessions::ADMIN)
        .json(&json!({ "supplier_name": "Mine now" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete("/v1/suppliers/302")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/suppliers/302/files")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

// ---- Price files

async fn upload_csv(server: &TestServer, storage: &Storage, body: &str) -> String {
    let res = server
        .post("/v1/uploads")
        .with_session(sessions::ADMIN)
        .json(&json!({ "folder": "supplier-files", "file_name": "list.csv", "content_type": "text/csv" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let url = res.json::<Value>()["url"].as_str().unwrap().to_string();
    storage
        .put(
            &storage.key_for_url(&url).unwrap(),
            body.as_bytes().to_vec(),
            "text/csv",
        )
        .await
        .unwrap();
    url
}

#[test_db::test(seed)]
async fn admins_add_list_and_delete_price_files(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    let url = upload_csv(&server, &storage, "color,price\nAlaska,$10").await;
    let res = server
        .post("/v1/suppliers/301/files")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "2026 list", "url": url }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let file_id = res.json::<Value>()["id"].as_i64().unwrap();

    let files: Value = server
        .get("/v1/suppliers/301/files")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(files.as_array().unwrap().len(), 1);
    assert_eq!(files[0]["id"], file_id);
    assert_eq!(files[0]["name"], "2026 list");
    assert_eq!(files[0]["url"], url);
    assert_eq!(files[0]["status"], "pending");
    assert_eq!(files[0]["error_token"], Value::Null);
    assert!(files[0]["create_date"].as_str().unwrap().ends_with('Z'));

    // Other company's admin cannot delete it.
    sqlx::query("UPDATE users SET is_admin = 1 WHERE id = 106")
        .execute(&pool)
        .await
        .unwrap();
    server
        .delete(&format!("/v1/supplier-files/{file_id}"))
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    server
        .delete(&format!("/v1/supplier-files/{file_id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(
        !storage
            .memory_keys()
            .contains(&storage.key_for_url(&url).unwrap())
    );
    let files: Value = server
        .get("/v1/suppliers/301/files")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(files, json!([]));
}

#[test_db::test(seed)]
async fn price_files_must_be_uploaded_files_of_an_accepted_type(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool);
    // Not uploaded to the bucket's supplier-files folder.
    server
        .post("/v1/suppliers/301/files")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "x", "url": "https://elsewhere.example/list.csv" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let url = upload_csv(&server, &storage, "a,b").await;
    server
        .post("/v1/suppliers/301/files")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "  ", "url": url }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    // Word files are not price files.
    server
        .post("/v1/uploads")
        .with_session(sessions::ADMIN)
        .json(&json!({ "folder": "supplier-files", "file_name": "a.docx", "content_type": "application/msword" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    // Only admins upload price files.
    server
        .post("/v1/uploads")
        .with_session(sessions::REP)
        .json(&json!({ "folder": "supplier-files", "file_name": "a.csv", "content_type": "text/csv" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn deleting_a_supplier_deletes_its_files(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    let created: Value = server
        .post("/v1/suppliers")
        .with_session(sessions::ADMIN)
        .json(&json!({ "supplier_name": "Short lived" }))
        .await
        .json();
    let id = created["id"].as_i64().unwrap();
    let url = upload_csv(&server, &storage, "a,b").await;
    server
        .post(&format!("/v1/suppliers/{id}/files"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "list", "url": url }))
        .await
        .assert_status(StatusCode::CREATED);
    server
        .delete(&format!("/v1/suppliers/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supplier_files WHERE supplier_id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
    assert!(
        !storage
            .memory_keys()
            .contains(&storage.key_for_url(&url).unwrap())
    );
}

// ---- CloudTalk address book

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

async fn cloudtalk_stub(existing: Option<&'static str>) -> (String, Calls) {
    let calls: Calls = Arc::default();
    let record =
        |calls: &Calls, what: String, body: Value| calls.lock().unwrap().push((what, body));
    let c1 = calls.clone();
    let c2 = calls.clone();
    let c3 = calls.clone();
    let router = Router::new()
        .route(
            "/contacts/index.json",
            get(
                move |Query(query): Query<HashMap<String, String>>| async move {
                    let data = existing.map_or_else(Vec::new, |industry| {
                        vec![json!({ "Contact": { "id": "77", "industry": industry } })]
                    });
                    assert!(query["industry"].starts_with("gm-supplier-100-"));
                    Json(json!({ "responseData": { "data": data } }))
                },
            ),
        )
        .route(
            "/contacts/add.json",
            put(move |Json(body): Json<Value>| async move {
                record(&c1, "add".into(), body);
                Json(json!({ "responseData": { "data": { "id": 88 } } }))
            }),
        )
        .route(
            "/contacts/edit/{id}",
            post(
                move |Path(id): Path<String>, Json(body): Json<Value>| async move {
                    record(&c2, format!("edit {id}"), body);
                    Json(json!({ "responseData": { "status": 200 } }))
                },
            ),
        )
        .route(
            "/contacts/delete/{id}",
            delete(move |Path(id): Path<String>| async move {
                record(&c3, format!("delete {id}"), Value::Null);
                Json(json!({ "responseData": { "status": 200 } }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}"), calls)
}

async fn with_cloudtalk(pool: &MySqlPool) {
    sqlx::query("UPDATE company SET cloudtalk_access_key = 'k', cloudtalk_access_secret = 's' WHERE id = 100")
        .execute(pool)
        .await
        .unwrap();
}

#[test_db::test(seed)]
async fn a_new_supplier_with_a_phone_is_added_to_cloudtalk(pool: MySqlPool) {
    with_cloudtalk(&pool).await;
    let (base, calls) = cloudtalk_stub(None).await;
    let server = TestServer::new(app(
        AppState::new(pool, SECRET, ALLOWED_ORIGIN).with_cloudtalk_base(&base)
    ));
    let created: Value = server
        .post("/v1/suppliers")
        .with_session(sessions::ADMIN)
        .json(&json!({
            "supplier_name": "Granite World",
            "manager": "Gina",
            "phone": "(317) 555-0100",
            "email": "sales@granite.example",
            "website": "https://granite.example"
        }))
        .await
        .json();
    let id = created["id"].as_i64().unwrap();
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "add");
    assert_eq!(
        calls[0].1,
        json!({
            "name": "Granite World",
            "industry": format!("gm-supplier-100-{id}"),
            "company": "Granite World",
            "ContactNumber": [{ "public_number": "+13175550100" }],
            "title": "Gina",
            "website": "https://granite.example",
            "ContactEmail": [{ "email": "sales@granite.example" }]
        })
    );
}

#[test_db::test(seed)]
async fn editing_updates_the_contact_and_removing_the_phone_deletes_it(pool: MySqlPool) {
    with_cloudtalk(&pool).await;
    let (base, calls) = cloudtalk_stub(Some("gm-supplier-100-301")).await;
    let server = TestServer::new(app(
        AppState::new(pool, SECRET, ALLOWED_ORIGIN).with_cloudtalk_base(&base)
    ));
    server
        .put("/v1/suppliers/301")
        .with_session(sessions::ADMIN)
        .json(&json!({ "supplier_name": "Quartz Partners", "phone": "3175550101" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .put("/v1/suppliers/301")
        .with_session(sessions::ADMIN)
        .json(&json!({ "supplier_name": "Quartz Partners", "phone": "" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let calls: Vec<String> = calls.lock().unwrap().iter().map(|c| c.0.clone()).collect();
    assert_eq!(calls, ["edit 77.json", "delete 77.json"]);
}

#[test_db::test(seed)]
async fn without_cloudtalk_keys_nothing_is_synced(pool: MySqlPool) {
    let (base, calls) = cloudtalk_stub(None).await;
    let server = TestServer::new(app(
        AppState::new(pool, SECRET, ALLOWED_ORIGIN).with_cloudtalk_base(&base)
    ));
    server
        .post("/v1/suppliers")
        .with_session(sessions::ADMIN)
        .json(&json!({ "supplier_name": "Granite World", "phone": "3175550100" }))
        .await
        .assert_status(StatusCode::CREATED);
    assert!(calls.lock().unwrap().is_empty());
}

// ---- The processor (port of supplierFileProcessor.test.ts)

#[derive(Default)]
struct Fake {
    files: HashMap<String, (Vec<u8>, String)>,
    text_reply: Option<String>,
    pdf_reply: Option<String>,
    image_reply: Option<String>,
    calls: Mutex<Vec<String>>,
}

impl Extractor for Fake {
    async fn download(&self, url: &str) -> Option<Downloaded> {
        let (bytes, content_type) = self.files.get(url)?.clone();
        Some(Downloaded {
            bytes,
            content_type,
            filename: url.rsplit('/').next().unwrap().to_string(),
        })
    }
    async fn structure_text(&self, text: &str, _: Option<i32>) -> Option<String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("text:{}", text.len()));
        self.text_reply.clone()
    }
    async fn structure_pdf_batch(
        &self,
        _: &[u8],
        _: &str,
        label: &str,
        _: Option<i32>,
    ) -> Option<String> {
        self.calls.lock().unwrap().push(format!("pdf:{label}"));
        self.pdf_reply
            .clone()
            .map(|reply| format!("### {label}\n{reply}"))
    }
    async fn structure_image(&self, url: &str, _: Option<i32>) -> Option<String> {
        self.calls.lock().unwrap().push(format!("image:{url}"));
        self.image_reply.clone()
    }
}

async fn add_file(pool: &MySqlPool, id: i64, url: &str, status: &str, attempts: i32) {
    sqlx::query(
        "INSERT INTO supplier_files (id, name, url, supplier_id, status, attempts, status_updated_at)
         VALUES (?, 'list', ?, 301, ?, ?, NOW())",
    )
    .bind(id)
    .bind(url)
    .bind(status)
    .bind(attempts)
    .execute(pool)
    .await
    .unwrap();
}

async fn file_state(pool: &MySqlPool, id: i64) -> (String, i32, Option<String>) {
    sqlx::query_as(
        "SELECT CAST(status AS CHAR), CAST(attempts AS SIGNED), error_token FROM supplier_files WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn chunks(pool: &MySqlPool, id: i64) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT heading, content FROM supplier_file_chunks WHERE supplier_file_id = ? ORDER BY chunk_index",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A PDF with `pages` pages, each saying `text` (empty: no text layer).
fn pdf(pages: usize, text: &str) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    });
    let resources_id = doc.add_object(dictionary! { "Font" => dictionary! { "F1" => font_id } });
    let mut kids = Vec::new();
    for _ in 0..pages {
        let operations = if text.is_empty() {
            Vec::new()
        } else {
            vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 10.into()]),
                Operation::new("Td", vec![20.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal(text)]),
                Operation::new("ET", vec![]),
            ]
        };
        let content = Content { operations };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        kids.push(page_id.into());
    }
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => kids, "Count" => i64::try_from(pages).unwrap(),
            "Resources" => resources_id,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

const TABLE: &str = "| color/product | thickness | group/level | slab size | price |\n|---|---|---|---|---|\n| Alaska White | 3 cm | 2 | 130x65 | $21 |";

#[test_db::test(seed)]
async fn a_csv_is_chunked_without_the_model(pool: MySqlPool) {
    let fake = Fake {
        files: HashMap::from([(
            "s3/list.csv".to_string(),
            (
                b"color,size,price\nAlaska,130x65,$21\nBlack,-,$14".to_vec(),
                "text/csv".into(),
            ),
        )]),
        ..Fake::default()
    };
    add_file(&pool, 9701, "s3/list.csv", "pending", 0).await;
    assert!(run_once(&pool, &fake).await.unwrap());
    assert_eq!(file_state(&pool, 9701).await, ("ready".into(), 1, None));
    assert_eq!(
        chunks(&pool, 9701).await,
        [(
            String::new(),
            "| color | size | price |\n|---|---|---|\n| Alaska | 130x65 | $21 |\n| Black | 130x65 | $14 |".into()
        )]
    );
    assert!(fake.calls.lock().unwrap().is_empty());
    let version: i32 = sqlx::query_scalar(
        "SELECT CAST(pipeline_version AS SIGNED) FROM supplier_files WHERE id = 9701",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(version, PIPELINE_VERSION);
    // Nothing else waits.
    assert!(!run_once(&pool, &fake).await.unwrap());
}

#[test_db::test(seed)]
async fn a_text_file_is_chunked_and_an_empty_one_retried_then_failed(pool: MySqlPool) {
    let fake = Fake {
        files: HashMap::from([
            (
                "s3/a.txt".to_string(),
                (b"Alaska $21\nBlack $14".to_vec(), "text/plain".into()),
            ),
            (
                "s3/empty.txt".to_string(),
                (b"  \n ".to_vec(), "text/plain".into()),
            ),
        ]),
        ..Fake::default()
    };
    add_file(&pool, 9702, "s3/a.txt", "pending", 0).await;
    add_file(&pool, 9701, "s3/empty.txt", "pending", 0).await;
    run_once(&pool, &fake).await.unwrap();
    assert_eq!(
        chunks(&pool, 9702).await,
        [(String::new(), "Alaska $21\nBlack $14".into())]
    );
    run_once(&pool, &fake).await.unwrap();
    assert_eq!(
        file_state(&pool, 9701).await,
        ("pending".into(), 1, Some("empty_result".into()))
    );
    sqlx::query("UPDATE supplier_files SET attempts = ? WHERE id = 9701")
        .bind(MAX_ATTEMPTS - 1)
        .execute(&pool)
        .await
        .unwrap();
    run_once(&pool, &fake).await.unwrap();
    assert_eq!(
        file_state(&pool, 9701).await,
        ("failed".into(), MAX_ATTEMPTS, Some("empty_result".into()))
    );
    assert!(chunks(&pool, 9701).await.is_empty());
}

#[test]
fn pdfs_are_split_into_batches_of_eight_pages() {
    let (pages, batches) = split_pdf(&pdf(19, "")).unwrap();
    assert_eq!(pages, 19);
    let labels: Vec<&str> = batches.iter().map(|b| b.page_label.as_str()).collect();
    assert_eq!(labels, ["pages 1-8", "pages 9-16", "pages 17-19"]);
    let counts: Vec<usize> = batches
        .iter()
        .map(|b| Document::load_mem(&b.bytes).unwrap().get_pages().len())
        .collect();
    assert_eq!(counts, [8, 8, 3]);
    assert!(split_pdf(b"not a pdf").is_none());
}

#[test_db::test(seed)]
async fn a_scanned_pdf_goes_to_the_model_page_batch_by_batch_in_order(pool: MySqlPool) {
    let fake = Fake {
        files: HashMap::from([(
            "s3/scan.pdf".to_string(),
            (pdf(10, ""), "application/pdf".into()),
        )]),
        pdf_reply: Some(TABLE.into()),
        ..Fake::default()
    };
    add_file(&pool, 9701, "s3/scan.pdf", "pending", 0).await;
    run_once(&pool, &fake).await.unwrap();
    assert_eq!(file_state(&pool, 9701).await.0, "ready");
    let mut calls = fake.calls.lock().unwrap().clone();
    calls.sort();
    assert_eq!(calls, ["pdf:pages 1-8", "pdf:pages 9-10"]);
    let headings: Vec<String> = chunks(&pool, 9701).await.into_iter().map(|c| c.0).collect();
    assert_eq!(headings, ["pages 1-8", "pages 9-10"]);
}

#[test_db::test(seed)]
async fn a_pdf_with_text_is_structured_from_its_text(pool: MySqlPool) {
    let line = "Alaska White 3cm Group 2 130x65 $21.00 Black Pearl 2cm Group 1 126x63 $14.50 ";
    let fake = Fake {
        files: HashMap::from([(
            "s3/text.pdf".to_string(),
            (pdf(2, &line.repeat(4)), "application/pdf".into()),
        )]),
        text_reply: Some(TABLE.into()),
        ..Fake::default()
    };
    add_file(&pool, 9701, "s3/text.pdf", "pending", 0).await;
    run_once(&pool, &fake).await.unwrap();
    assert_eq!(file_state(&pool, 9701).await.0, "ready");
    let calls = fake.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("text:"));
}

#[test_db::test(seed)]
async fn a_blank_model_answer_retries_then_fails(pool: MySqlPool) {
    let fake = Fake {
        files: HashMap::from([(
            "s3/scan.pdf".to_string(),
            (pdf(1, ""), "application/pdf".into()),
        )]),
        ..Fake::default()
    };
    add_file(&pool, 9701, "s3/scan.pdf", "pending", 0).await;
    run_once(&pool, &fake).await.unwrap();
    // Asked twice for the batch.
    assert_eq!(fake.calls.lock().unwrap().len(), 2);
    assert_eq!(
        file_state(&pool, 9701).await,
        ("pending".into(), 1, Some("empty_result".into()))
    );
}

#[test_db::test(seed)]
async fn pictures_go_to_the_model_by_url(pool: MySqlPool) {
    let fake = Fake {
        files: HashMap::from([
            (
                "s3/list.png".to_string(),
                (vec![1, 2, 3], "image/png".into()),
            ),
            (
                "s3/blank.jpg".to_string(),
                (vec![1, 2, 3], "image/jpeg".into()),
            ),
        ]),
        image_reply: Some(TABLE.into()),
        ..Fake::default()
    };
    add_file(&pool, 9702, "s3/list.png", "pending", 0).await;
    run_once(&pool, &fake).await.unwrap();
    assert_eq!(file_state(&pool, 9702).await.0, "ready");
    assert_eq!(fake.calls.lock().unwrap().clone(), ["image:s3/list.png"]);
    let fake = Fake {
        image_reply: None,
        ..fake
    };
    add_file(&pool, 9701, "s3/blank.jpg", "pending", 0).await;
    run_once(&pool, &fake).await.unwrap();
    assert_eq!(
        file_state(&pool, 9701).await.2.as_deref(),
        Some("empty_result")
    );
}

#[test_db::test(seed)]
async fn unsupported_and_missing_files_fail(pool: MySqlPool) {
    let fake = Fake {
        files: HashMap::from([(
            "s3/list.docx".to_string(),
            (vec![1], "application/msword".into()),
        )]),
        ..Fake::default()
    };
    add_file(&pool, 9702, "s3/list.docx", "pending", 0).await;
    add_file(&pool, 9701, "s3/gone.csv", "pending", 0).await;
    run_once(&pool, &fake).await.unwrap();
    assert_eq!(
        file_state(&pool, 9702).await,
        ("failed".into(), 1, Some("unsupported_format".into()))
    );
    run_once(&pool, &fake).await.unwrap();
    assert_eq!(
        file_state(&pool, 9701).await,
        ("pending".into(), 1, Some("download_failed".into()))
    );
}

#[test_db::test(seed)]
async fn an_unchanged_file_keeps_its_chunks(pool: MySqlPool) {
    let fake = Fake {
        files: HashMap::from([(
            "s3/a.txt".to_string(),
            (b"Alaska $21".to_vec(), "text/plain".into()),
        )]),
        ..Fake::default()
    };
    add_file(&pool, 9701, "s3/a.txt", "pending", 0).await;
    run_once(&pool, &fake).await.unwrap();
    sqlx::query("UPDATE supplier_file_chunks SET content = 'kept' WHERE supplier_file_id = 9701")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE supplier_files SET status = 'pending' WHERE id = 9701")
        .execute(&pool)
        .await
        .unwrap();
    run_once(&pool, &fake).await.unwrap();
    assert_eq!(chunks(&pool, 9701).await, [(String::new(), "kept".into())]);
    assert_eq!(file_state(&pool, 9701).await.0, "ready");
}

#[test_db::test(seed)]
async fn the_newest_waiting_file_is_claimed_and_old_pipelines_reprocessed(pool: MySqlPool) {
    add_file(&pool, 9701, "s3/a.txt", "pending", 0).await;
    add_file(&pool, 9702, "s3/b.txt", "pending", 0).await;
    add_file(&pool, 9703, "s3/c.txt", "ready", 1).await;
    sqlx::query("UPDATE supplier_files SET pipeline_version = ? WHERE id = 9703")
        .bind(PIPELINE_VERSION - 1)
        .execute(&pool)
        .await
        .unwrap();
    let claimed: ClaimedFile = claim_next(&pool).await.unwrap().unwrap();
    assert_eq!(claimed.id, 9703);
    assert_eq!(claimed.attempts, 2);
    assert_eq!(claimed.company_id, Some(100));
    assert_eq!(claim_next(&pool).await.unwrap().unwrap().id, 9702);
    assert_eq!(
        file_state(&pool, 9702).await,
        ("processing".into(), 1, None)
    );
}

#[test_db::test(seed)]
async fn stale_processing_files_are_reclaimed_or_failed(pool: MySqlPool) {
    add_file(&pool, 9701, "s3/a.txt", "processing", 1).await;
    add_file(&pool, 9702, "s3/b.txt", "processing", MAX_ATTEMPTS).await;
    add_file(&pool, 9703, "s3/c.txt", "processing", 1).await;
    sqlx::query("UPDATE supplier_files SET status_updated_at = NOW() - INTERVAL 11 MINUTE WHERE id IN (9701, 9702)")
        .execute(&pool)
        .await
        .unwrap();
    reclaim_stale(&pool).await.unwrap();
    assert_eq!(file_state(&pool, 9701).await.0, "pending");
    assert_eq!(
        file_state(&pool, 9702).await,
        ("failed".into(), MAX_ATTEMPTS, Some("stalled".into()))
    );
    assert_eq!(file_state(&pool, 9703).await.0, "processing");
}

#[test_db::test(seed)]
async fn the_job_needs_its_key_and_processes_the_queue(pool: MySqlPool) {
    let storage = Storage::memory();
    storage
        .put(
            "supplier-files/list.csv",
            b"color,price\nAlaska,$21".to_vec(),
            "text/csv",
        )
        .await
        .unwrap();
    let url = storage.public_url("supplier-files/list.csv").unwrap();
    add_file(&pool, 9701, &url, "pending", 0).await;
    let server = TestServer::new(app(AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_storage(storage)
        .with_job_key("job-key")));
    server
        .post("/v1/jobs/supplier-files")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    let run: Value = server
        .post("/v1/jobs/supplier-files")
        .add_header("Authorization", "job-key")
        .await
        .json();
    assert_eq!(run, json!({ "processed": 1 }));
    assert_eq!(file_state(&pool, 9701).await.0, "ready");
}
