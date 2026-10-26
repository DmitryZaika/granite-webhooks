use crate::common;

use ::common::telephony::ringcentral_auth::encrypt;
use api::app;
use api::qbo::Qbo;
use api::state::AppState;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const KEY: [u8; 32] = [7; 32];

// Company 100 is connected to realm 555. Alice Lead (1000, alice@example.com)
// is QuickBooks customer 42, which owns invoice 9 but not invoice 10.

fn invoice() -> Value {
    json!({
        "Id": "9", "SyncToken": "1", "DocNumber": "1001", "TxnDate": "2026-05-01", "TotalAmt": 120,
        "CustomerRef": { "value": "42" },
        "Line": [
            { "Id": "1", "DetailType": "SalesItemLineDetail", "Amount": 120, "Description": "Quartz",
              "SalesItemLineDetail": { "ItemRef": { "value": "3", "name": "Stone" }, "Qty": 1, "UnitPrice": 120 } },
            { "DetailType": "SubTotalLineDetail", "Amount": 120 },
        ],
    })
}

async fn server(pool: &MySqlPool, access_expires_in: i64) -> (TestServer, Arc<Mutex<Vec<Value>>>) {
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "UPDATE company SET qbo_realm_id = '555', qbo_access_token = ?, qbo_refresh_token = ?,
                qbo_access_expires = ?, qbo_refresh_expires = ?, qbo_client_id = ?, qbo_client_secret = ?
          WHERE id = 100",
    )
    .bind(encrypt(&KEY, "old-access"))
    .bind(encrypt(&KEY, "refresh-1"))
    .bind(now + access_expires_in)
    .bind(now + 86_400)
    .bind(encrypt(&KEY, "client-id-123"))
    .bind(encrypt(&KEY, "client-secret-123"))
    .execute(pool)
    .await
    .unwrap();
    let posted = Arc::new(Mutex::new(Vec::new()));
    let saved = Arc::clone(&posted);
    let router = Router::new()
        .route(
            "/token",
            post(|body: String| async move {
                assert_eq!(body, "grant_type=refresh_token&refresh_token=refresh%2D1");
                Json(json!({ "access_token": "new-access", "refresh_token": "refresh-2",
                             "expires_in": 3600, "x_refresh_token_expires_in": 8_640_000 }))
            }),
        )
        .route(
            "/v3/company/555/query",
            get(|Query(query): Query<HashMap<String, String>>, headers: axum::http::HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer new-access");
                let q = &query["query"];
                if q.contains("PrimaryEmailAddr = 'alice@example.com'") {
                    return Json(json!({ "QueryResponse": { "Customer": [{ "Id": "42", "DisplayName": "Alice Lead" }] } }));
                }
                if q.starts_with("select Id, CustomerRef from Invoice where Id = '9'") {
                    return Json(json!({ "QueryResponse": { "Invoice": [{ "Id": "9", "CustomerRef": { "value": "42" } }] } }));
                }
                if q.starts_with("select Id, CustomerRef from Invoice") {
                    return Json(json!({ "QueryResponse": { "Invoice": [{ "Id": "10", "CustomerRef": { "value": "77" } }] } }));
                }
                if q == "select * from Class MAXRESULTS 1000" {
                    return Json(json!({ "QueryResponse": { "Class": [
                        { "Id": "1", "Name": "Rita", "FullyQualifiedName": "Rita" },
                        { "Id": "2", "Name": "Kitchens", "FullyQualifiedName": "Rita:Kitchens" },
                        { "Id": "3", "Name": "Mark", "FullyQualifiedName": "Mark" },
                    ] } }));
                }
                if q.contains("from Invoice where TxnDate >=") {
                    return Json(json!({ "QueryResponse": { "Invoice": [
                        { "Id": "9", "DocNumber": "1001", "TxnDate": "2026-05-02", "TotalAmt": 120, "CustomerRef": { "value": "42", "name": "Alice Lead" } },
                    ] } }));
                }
                if q.contains("where CustomerRef = '42'") {
                    return Json(json!({ "QueryResponse": { "Invoice": [
                        { "Id": "8", "TxnDate": "2026-04-01", "TotalAmt": 50, "Balance": 0, "ShipAddr": { "Line1": "1 Main", "City": "Carmel" } },
                        { "Id": "9", "TxnDate": "2026-05-01", "TotalAmt": 120, "Balance": "120" },
                    ] } }));
                }
                Json(json!({ "QueryResponse": {} }))
            }),
        )
        .route("/v3/company/555/invoice/{id}", get(|Path(id): Path<String>| async move {
            assert_eq!(id, "9");
            Json(json!({ "Invoice": invoice() }))
        }))
        .route(
            "/v3/company/555/reports/ClassSales",
            get(|Query(query): Query<HashMap<String, String>>| async move {
                assert_eq!(query["accounting_method"], "Accrual");
                Json(json!({ "Header": { "Currency": "USD", "StartPeriod": query["start_date"] },
                    "Rows": { "Row": [
                        { "ColData": [{ "value": "Rita" }, { "value": "1,000" }] },
                        { "ColData": [{ "value": "Rita/Kitchens" }, { "value": "250.5" }] },
                        { "ColData": [{ "value": "Mark" }, { "value": "99" }] },
                    ] } }))
            }),
        )
        .route(
            "/v3/company/555/reports/CustomerSales",
            get(|Query(query): Query<HashMap<String, String>>| async move {
                assert_eq!(query["class"], "1,2");
                Json(json!({ "Header": { "Currency": "USD" }, "Rows": { "Row": [
                    { "ColData": [{ "value": "Alice Lead", "id": "42" }, { "value": "120" }] },
                    { "ColData": [{ "value": "Zero Co", "id": "43" }, { "value": "0" }] },
                    { "group": "GrandTotal", "Summary": { "ColData": [{ "value": "TOTAL" }, { "value": "120" }] } },
                ] } }))
            }),
        )
        .route("/v3/company/555/invoice/{id}/pdf", get(|| async { ([("content-type", "application/pdf")], b"%PDF-qbo".to_vec()) }))
        .route(
            "/v3/company/555/invoice",
            post(move |Json(body): Json<Value>| {
                let saved = Arc::clone(&saved);
                async move {
                    saved.lock().unwrap().push(body.clone());
                    let mut updated = invoice();
                    updated["SyncToken"] = json!("2");
                    updated["Line"] = body["Line"].clone();
                    Json(json!({ "Invoice": updated }))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let qbo = Qbo::new(Some(KEY), None, &base, &format!("{base}/token"), false);
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN).with_qbo(qbo);
    (TestServer::new(app(state)), posted)
}

#[test_db::test(seed)]
async fn invoices_link_the_customer_and_refresh_the_token(pool: MySqlPool) {
    let (server, _) = server(&pool, 10).await;
    let body: Value = server
        .get("/v1/customers/1000/qbo-invoices")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["qbo_id"], "42");
    assert_eq!(body["invoices"][0]["id"], "9");
    assert_eq!(body["invoices"][0]["balance"], 120.0);
    assert_eq!(body["invoices"][1]["project_address"], "1 Main, Carmel");
    let linked: Option<String> =
        sqlx::query_scalar("SELECT CAST(qbo_id AS CHAR) FROM customers WHERE id = 1000")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(linked.as_deref(), Some("42"));
    let (token, expires): (Vec<u8>, i64) =
        sqlx::query_as("SELECT qbo_access_token, qbo_access_expires FROM company WHERE id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        ::common::telephony::ringcentral_auth::decrypt(&KEY, &token).as_deref(),
        Some("new-access")
    );
    assert!(expires > chrono::Utc::now().timestamp() + 3000);
}

#[test_db::test(seed)]
async fn an_invoice_is_edited_and_its_pdf_read(pool: MySqlPool) {
    sqlx::query("UPDATE customers SET qbo_id = '42' WHERE id = 1000")
        .execute(&pool)
        .await
        .unwrap();
    let (server, posted) = server(&pool, 10).await;
    let mut invoice: Value = server
        .get("/v1/customers/1000/qbo-invoices/9")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(invoice["sync_token"], "1");
    assert_eq!(invoice["lines"][0]["item_ref_name"], "Stone");
    invoice["lines"][0]["qty"] = json!(2);
    invoice["lines"][0]["amount"] = Value::Null;
    let saved: Value = server
        .put("/v1/customers/1000/qbo-invoices/9")
        .with_session(sessions::REP)
        .json(&json!({ "invoice": invoice }))
        .await
        .json();
    assert_eq!(saved["sync_token"], "2");
    assert_eq!(posted.lock().unwrap()[0]["Line"][0]["Amount"], 240.0);
    let pdf = server
        .get("/v1/customers/1000/qbo-invoices/9/pdf")
        .with_session(sessions::REP)
        .await;
    pdf.assert_status_ok();
    assert_eq!(pdf.as_bytes().as_ref(), b"%PDF-qbo");
    server
        .get("/v1/customers/1000/qbo-invoices/10/pdf")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/qbo/invoices/10/pdf")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn a_company_without_quickbooks_says_so(pool: MySqlPool) {
    let body: Value = common::server(pool)
        .get("/v1/customers/1000/qbo-invoices")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        body,
        json!({ "status": "not_connected", "invoices": [], "qbo_id": null })
    );
}

#[test_db::test(seed)]
async fn a_rep_sees_their_class_sales_and_customers(pool: MySqlPool) {
    let (server, _) = server(&pool, 3600).await;
    sqlx::query("UPDATE company SET qbo_access_token = ? WHERE id = 100")
        .bind(encrypt(&KEY, "new-access"))
        .execute(&pool)
        .await
        .unwrap();
    let sales: Value = server
        .get("/v1/me/qbo-sales")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(sales["status"], "ok");
    assert_eq!(sales["amount"], 1250.5);
    assert_eq!(sales["currency"], "USD");
    assert!(sales["days_remaining_in_month"].as_u64().unwrap() >= 1);
    let customers: Value = server
        .get("/v1/me/qbo-sales/customers")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(customers["status"], "ok");
    assert_eq!(customers["total"], 120.0);
    assert_eq!(customers["customers"].as_array().unwrap().len(), 1);
    assert_eq!(customers["customers"][0]["qbo_customer_id"], "42");
    assert_eq!(
        customers["customers"][0]["invoices"][0]["doc_number"],
        "1001"
    );
    let other: Value = common::server(pool)
        .get("/v1/me/qbo-sales")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(other["status"], "unavailable");
}
