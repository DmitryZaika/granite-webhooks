//! Signed documents opened in a browser tab: native documents from the
//! private signature bucket, and `SignWell` signed PDFs.

mod common;

use api::app;
use api::state::AppState;
use api::storage::Storage;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::MySqlPool;

const DOC: &str = "24a9c706-2147-4b06-ae24-ff4a2c4c520c";

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn a_pdf(text: &str) -> Vec<u8> {
    let mut page = api::pdf::Page::new();
    page.text(text, 72.0, 700.0, 20.0, api::pdf::Font::Bold);
    page.finish()
}

/// A SignWell stand-in: `doc-1` is signed, its PDF is served by the stub.
async fn signwell(signed_pdf: Vec<u8>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let file_url = format!("{base}/files/doc-1.pdf");
    let router = Router::new()
        .route(
            "/documents/{id}/completed_pdf",
            get(move |Path(id): Path<String>| {
                let file_url = file_url.clone();
                async move {
                    assert_eq!(id, "doc-1");
                    Json(json!({ "file_url": file_url }))
                }
            }),
        )
        .route("/files/doc-1.pdf", get(move || async move { signed_pdf }));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

async fn server(pool: MySqlPool, signwell_base: &str) -> (TestServer, Storage) {
    let storage = Storage::memory();
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN)
        .with_storage(storage.clone())
        .with_signature_bucket("signatures-private")
        .with_signwell_base(signwell_base);
    (TestServer::new(app(state)), storage)
}

/// Request 9201 (by Rita, user 100, for Alice) sent natively; `signed`
/// moves it to its completed file.
async fn native_request(pool: &MySqlPool, storage: &Storage, signed: bool) -> (Vec<u8>, Vec<u8>) {
    let prepared = a_pdf("Prepared contract");
    let completed = a_pdf("Signed contract");
    let (p_hash, c_hash) = (sha(&prepared), sha(&completed));
    let prefix = format!("100/signatures/{DOC}/");
    storage
        .put(
            &format!("{prefix}prepared-{p_hash}.pdf"),
            prepared.clone(),
            "application/pdf",
        )
        .await
        .unwrap();
    storage
        .put(
            &format!("{prefix}completed-{c_hash}.pdf"),
            completed.clone(),
            "application/pdf",
        )
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO signature_requests (id, company_id, customer_id, created_by_user_id, filename, file_url,
             signer_name, signer_email, status, provider, provider_document_id)
         VALUES (9201, 100, 1000, 100, 'contract.pdf', 'https://files.example/original.pdf', 'Alice Lead',
             'alice@example.com', ?, 'native', ?)",
    )
    .bind(if signed { "signed" } else { "sent" })
    .bind(DOC)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO native_signature_documents (request_id, company_id, document_id, prepared_object_key,
             prepared_sha256, document_version, token_hash, fields_json, settings_json,
             completed_object_key, completed_sha256)
         VALUES (9201, 100, ?, ?, ?, ?, ?, '[]', '{}', ?, ?)",
    )
    .bind(DOC)
    .bind(format!("{prefix}prepared-{p_hash}.pdf"))
    .bind(&p_hash)
    .bind(&p_hash)
    .bind(sha(b"token"))
    .bind(signed.then(|| format!("{prefix}completed-{c_hash}.pdf")))
    .bind(signed.then(|| c_hash.clone()))
    .execute(pool)
    .await
    .unwrap();
    (prepared, completed)
}

// ---- GET /v1/signatures/{id}/document

#[test_db::test(seed)]
async fn a_signed_native_document_opens_its_completed_file(pool: MySqlPool) {
    let (server, storage) = server(pool.clone(), "http://127.0.0.1:9").await;
    let (_, completed) = native_request(&pool, &storage, true).await;
    let response = server
        .get("/v1/signatures/9201/document")
        .with_session(sessions::REP)
        .await;
    response.assert_status_ok();
    assert_eq!(response.header("content-type"), "application/pdf");
    assert_eq!(
        response.header("content-disposition"),
        "inline; filename=\"contract.pdf\"; filename*=UTF-8''contract%2Epdf"
    );
    assert_eq!(response.as_bytes().to_vec(), completed);
}

#[test_db::test(seed)]
async fn a_sent_native_document_opens_as_prepared(pool: MySqlPool) {
    let (server, storage) = server(pool.clone(), "http://127.0.0.1:9").await;
    let (prepared, _) = native_request(&pool, &storage, false).await;
    let response = server
        .get("/v1/signatures/9201/document")
        .with_session(sessions::REP)
        .await;
    response.assert_status_ok();
    assert_eq!(response.as_bytes().to_vec(), prepared);
}

#[test_db::test(seed)]
async fn a_signed_document_without_its_completed_file_never_opens_the_unsigned_one(
    pool: MySqlPool,
) {
    let (server, storage) = server(pool.clone(), "http://127.0.0.1:9").await;
    native_request(&pool, &storage, true).await;
    sqlx::query(
        "UPDATE native_signature_documents SET completed_object_key = NULL WHERE request_id = 9201",
    )
    .execute(&pool)
    .await
    .unwrap();
    let response = server
        .get("/v1/signatures/9201/document")
        .with_session(sessions::REP)
        .await;
    response.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert!(response.text().contains("temporarily unavailable"));
}

#[test_db::test(seed)]
async fn a_tampered_file_is_not_served(pool: MySqlPool) {
    let (server, storage) = server(pool.clone(), "http://127.0.0.1:9").await;
    native_request(&pool, &storage, true).await;
    let key: String = sqlx::query_scalar(
        "SELECT completed_object_key FROM native_signature_documents WHERE request_id = 9201",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    storage
        .put(&key, b"%PDF-1.7 changed".to_vec(), "application/pdf")
        .await
        .unwrap();
    server
        .get("/v1/signatures/9201/document")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::SERVICE_UNAVAILABLE);
}

#[test_db::test(seed)]
async fn only_the_sender_an_admin_or_the_customers_page_opens_it(pool: MySqlPool) {
    let (server, storage) = server(pool.clone(), "http://127.0.0.1:9").await;
    native_request(&pool, &storage, true).await;
    // Another employee of the company, outside the customer's page.
    server
        .get("/v1/signatures/9201/document")
        .with_session(sessions::MANAGER)
        .await
        .assert_status_not_found();
    // From the customer's documents, anyone in the company.
    server
        .get("/v1/signatures/9201/document?customer_id=1000")
        .with_session(sessions::MANAGER)
        .await
        .assert_status_ok();
    server
        .get("/v1/signatures/9201/document?customer_id=1001")
        .with_session(sessions::MANAGER)
        .await
        .assert_status_not_found();
    server
        .get("/v1/signatures/9201/document")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
    // Another company never.
    server
        .get("/v1/signatures/9201/document?customer_id=1000")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status_not_found();
    let response = server.get("/v1/signatures/9201/document").await;
    response.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(response.text(), "Please sign in to view this document.");
}

// ---- GET /v1/signature-requests/{id}/signed-pdf

async fn signwell_request(pool: &MySqlPool, status: &str, file_url: &str) {
    sqlx::raw_sql("UPDATE company SET signwell_api_key = 'sw-key' WHERE id = 100")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO signature_requests (id, company_id, customer_id, created_by_user_id, filename, file_url,
             signer_name, signer_email, status, provider, provider_document_id)
         VALUES (9202, 100, 1000, 100, 'John Clay.pdf', ?, 'John Clay', 'john@example.com', ?, 'signwell', 'doc-1')",
    )
    .bind(file_url)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

fn title(bytes: &[u8]) -> String {
    let document = lopdf::Document::load_mem(bytes).expect("a PDF");
    let info = document
        .trailer
        .get(b"Info")
        .and_then(lopdf::Object::as_reference)
        .unwrap();
    let raw = document
        .get_object(info)
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"Title")
        .unwrap()
        .as_str()
        .unwrap()
        .to_vec();
    let units: Vec<u16> = raw[2..]
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).unwrap()
}

#[test_db::test(seed)]
async fn a_signed_signwell_pdf_opens_named_and_titled_signed(pool: MySqlPool) {
    let base = signwell(a_pdf("Signed at SignWell")).await;
    let (server, _) = server(pool.clone(), &base).await;
    signwell_request(&pool, "signed", "https://files.example/original.pdf").await;
    let response = server
        .get("/v1/signature-requests/9202/signed-pdf")
        .with_session(sessions::MANAGER)
        .await;
    response.assert_status_ok();
    assert_eq!(response.header("content-type"), "application/pdf");
    assert!(
        response
            .header("content-disposition")
            .to_str()
            .unwrap()
            .contains("filename=\"John Clay signed.pdf\"")
    );
    assert_eq!(title(response.as_bytes()), "John Clay signed");
}

#[test_db::test(seed)]
async fn an_unsigned_request_opens_the_original_file(pool: MySqlPool) {
    let (server, storage) = server(pool.clone(), "http://127.0.0.1:9").await;
    storage
        .put("emails/original.pdf", a_pdf("Original"), "application/pdf")
        .await
        .unwrap();
    signwell_request(
        &pool,
        "sent",
        &format!("{}/emails/original.pdf", api::storage::MEMORY_PUBLIC_BASE),
    )
    .await;
    let response = server
        .get("/v1/signature-requests/9202/signed-pdf")
        .with_session(sessions::REP)
        .await;
    response.assert_status_ok();
    assert_eq!(title(response.as_bytes()), "John Clay signed");
}

#[test_db::test(seed)]
async fn signed_pdfs_stay_in_the_company_and_need_a_session(pool: MySqlPool) {
    let (server, _) = server(pool.clone(), "http://127.0.0.1:9").await;
    signwell_request(&pool, "signed", "https://files.example/original.pdf").await;
    server
        .get("/v1/signature-requests/9202/signed-pdf")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status_not_found();
    let response = server.get("/v1/signature-requests/9202/signed-pdf").await;
    response.assert_status(StatusCode::UNAUTHORIZED);
    // SignWell unreachable and no stored original: a clear failure, not a hang.
    let response = server
        .get("/v1/signature-requests/9202/signed-pdf")
        .with_session(sessions::REP)
        .await;
    response.assert_status(StatusCode::BAD_GATEWAY);
}
