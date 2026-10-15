mod common;

use api::app;
use api::state::AppState;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn server(pool: MySqlPool) -> TestServer {
    let router = Router::new().route(
        "/documents/{id}",
        get(|Path(id): Path<String>, headers: HeaderMap| async move {
            assert_eq!(headers["x-api-key"], "sw-key");
            let status = if id == "doc-done" {
                "Completed"
            } else {
                "Sent"
            };
            Json(json!({ "status": status, "recipients": [{ "status": "viewed" }] }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN)
        .with_signwell_base(&format!("http://{address}"));
    TestServer::new(app(state))
}

#[test_db::test(seed)]
async fn open_requests_are_refreshed_from_signwell(pool: MySqlPool) {
    sqlx::raw_sql(
        "UPDATE company SET signwell_api_key = 'sw-key' WHERE id = 100;
         INSERT INTO signature_requests (id, company_id, customer_id, created_by_user_id, filename, file_url,
             signer_name, signer_email, status, provider, provider_document_id, created_at) VALUES
           (9101, 100, 1000, 100, 'contract.pdf', 'https://files.example/contract.pdf', 'Alice Lead',
            'alice@example.com', 'sent', 'signwell', 'doc-done', '2026-05-01 10:00:00'),
           (9102, 100, 1000, 100, 'addendum.pdf', 'https://files.example/addendum.pdf', 'Alice Lead',
            'alice@example.com', 'sent', 'signwell', 'doc-open', '2026-05-02 10:00:00'),
           (9103, 100, 1000, 100, 'native.pdf', 'https://files.example/native.pdf', 'Alice Lead',
            'alice@example.com', 'sent', 'native', NULL, '2026-05-03 10:00:00'),
           (9104, 100, 1000, 100, 'old.pdf', 'https://files.example/old.pdf', 'Alice Lead',
            'alice@example.com', 'declined', 'signwell', 'doc-done', '2026-04-01 10:00:00');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool).await;
    let body: Value = server
        .get("/v1/customers/1000/signature-requests")
        .with_session(sessions::REP)
        .await
        .json();
    let rows = body.as_array().unwrap();
    let ids: Vec<i64> = rows.iter().map(|row| row["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, [9103, 9102, 9101, 9104]);
    assert_eq!(
        rows[0]["view_url"],
        "/v1/signatures/9103/document?customer_id=1000"
    );
    assert_eq!(rows[0]["status"], "sent");
    assert_eq!(rows[1]["status"], "viewed");
    assert_eq!(rows[2]["status"], "signed");
    assert_eq!(
        rows[2]["view_url"],
        "/v1/signature-requests/9101/signed-pdf"
    );
    assert_eq!(
        rows[3]["status"], "declined",
        "a final status never changes"
    );
    assert_eq!(rows[3]["view_url"], "https://files.example/old.pdf");
    server
        .get("/v1/customers/1000/signature-requests")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
