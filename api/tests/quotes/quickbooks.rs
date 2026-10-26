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
                if q.starts_with("select * from Class where Active = true") {
                    return Json(json!({ "QueryResponse": { "Class": [
                        { "Id": "1", "Name": "Rita", "FullyQualifiedName": "Rita" },
                        { "Id": "2", "Name": "Kitchens", "FullyQualifiedName": "Rita:Kitchens" },
                        { "Id": "4", "Name": "Rita Rep", "FullyQualifiedName": "Rita Rep" },
                    ] } }));
                }
                if q == "select Id from Class where FullyQualifiedName = 'Rita'" {
                    return Json(json!({ "QueryResponse": { "Class": [{ "Id": "1" }] } }));
                }
                if q.starts_with("select * from Invoice ORDERBY MetaData.CreateTime DESC") {
                    let mut raw = invoice();
                    raw["ClassRef"] = json!({ "value": "1", "name": "Rita" });
                    return Json(json!({ "QueryResponse": { "Invoice": [raw] } }));
                }
                if q.starts_with("select * from Item where Active = true") {
                    return Json(json!({ "QueryResponse": { "Item": [
                        { "Id": "5", "Name": "tile", "Type": "Service" },
                        { "Id": "3", "Name": "Stone", "Type": "NonInventory" },
                        { "Id": "4", "Name": "Parts", "Type": "Category" },
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
async fn a_quote_makes_one_invoice_with_the_companys_items(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep,
                             company_id, salesperson_id, created_by, status)
         VALUES (9801, 1000, 'Kitchen', 'countertops', NOW(), 'Rep', 100, 100, 100, 'converted')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (server, posted) = server(&pool, 10).await;
    let items: Value = server
        .get("/v1/qbo/items")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        items["items"],
        json!([{ "id": "3", "name": "Stone" }, { "id": "5", "name": "tile" }])
    );
    assert_eq!(items["classes"], json!(["Rita", "Rita Rep", "Rita:Kitchens"]));
    let line = |id: &str, detail: &str, amount: Value, item: Value| {
        json!({ "id": id, "line_num": null, "detail_type": detail, "description": "Kitchen",
                "amount": amount, "qty": null, "unit_price": amount, "item_ref_value": item,
                "item_ref_name": null, "editable": true })
    };
    let mut opened = line("new:1", "DescriptionOnly", Value::Null, Value::Null);
    opened["description"] = json!("Flat edge");
    let body = json!({
        "quote_id": 9801,
        "draft": [line("new:0", "SalesItemLineDetail", json!(500), json!("3")), opened],
        "invoice": {
            "id": "", "sync_token": "", "doc_number": null, "txn_date": "2026-10-26",
            "due_date": null, "total_amt": 500, "balance": null, "currency": null,
            "customer_memo": null, "private_note": "Quote Kitchen", "bill_addr": null,
            "ship_addr": { "line1": "1 Main", "line2": null, "line3": null, "city": "Carmel",
                           "country_sub_division_code": "IN", "postal_code": "46032" },
            "lines": [
                line("new:0", "SalesItemLineDetail", json!(500), json!("3")),
                line("new:1", "DescriptionOnly", Value::Null, Value::Null),
            ],
            "class_name": "Rita",
            "bill_email_cc": "rep@acme.test",
        },
    });
    let created: Value = server
        .post("/v1/customers/1000/qbo-invoices")
        .with_session(sessions::REP)
        .json(&body)
        .await
        .json();
    assert_eq!(created["id"], "9");
    let sent = posted.lock().unwrap()[0].clone();
    assert_eq!(sent["CustomerRef"]["value"], "42");
    assert_eq!(sent["BillEmail"]["Address"], "alice@example.com");
    assert_eq!(sent["ShipAddr"]["City"], "Carmel");
    assert_eq!(sent["Line"][0]["SalesItemLineDetail"]["ItemRef"]["value"], "3");
    assert_eq!(sent["Line"][1]["DetailType"], "DescriptionOnly");
    let linked: Option<String> =
        sqlx::query_scalar("SELECT qbo_invoice_id FROM quotes WHERE id = 9801")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(linked.as_deref(), Some("9"));
    assert_eq!(sent["ClassRef"]["value"], "1");
    assert_eq!(sent["BillEmailCc"]["Address"], "rep@acme.test");
    let outcomes: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT field, outcome, saved_value FROM qbo_invoice_draft_outcomes
          WHERE company_id = 100 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let outcome = |field: &str, outcome: &str, value: Option<&str>| {
        (field.to_string(), outcome.to_string(), value.map(String::from))
    };
    assert_eq!(
        outcomes,
        [
            outcome("line", "kept", None),
            outcome("line", "changed", Some("Kitchen")),
            outcome("class", "value", Some("Rita")),
            outcome("cc", "sales_rep", None),
            outcome("item", "value", Some("3")),
        ]
    );
    server
        .post("/v1/customers/1000/qbo-invoices")
        .with_session(sessions::REP)
        .json(&body)
        .await
        .assert_status(StatusCode::CONFLICT);
    assert_eq!(posted.lock().unwrap().len(), 1);
}

#[test_db::test(seed)]
async fn a_draft_follows_what_recent_invoices_keep_doing(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep,
                             company_id, salesperson_id, created_by, status)
         VALUES (9802, 1000, 'Kitchen', 'countertops', NOW(), 'Rep', 100, 100, 100, 'converted')",
    )
    .execute(&pool)
    .await
    .unwrap();
    for _ in 0..4 {
        sqlx::query(
            "INSERT INTO qbo_invoice_draft_outcomes (company_id, field, draft_shape, draft_text, outcome, saved_value) VALUES
               (100, 'line', '# lin ft finished edge - flat', '33.5 lin ft Finished Edge - Flat', 'changed', 'Flat Edge'),
               (100, 'line', 'laser template', 'Laser Template', 'removed', NULL),
               (100, 'line', '# sink - 60/40', '1 Sink - 60/40', 'kept', NULL),
               (100, 'class', NULL, NULL, 'sales_rep', NULL),
               (100, 'cc', NULL, NULL, 'none', NULL),
               (100, 'item', NULL, 'tile', 'value', '5')",
        )
        .execute(&pool)
        .await
        .unwrap();
    }
    let (server, _) = server(&pool, 10).await;
    let line = |id: &str, detail: &str, text: &str, amount: Value| {
        json!({ "id": id, "line_num": null, "detail_type": detail, "description": text,
                "amount": amount, "qty": null, "unit_price": amount, "item_ref_value": "3",
                "item_ref_name": "Stone", "editable": true })
    };
    let draft: Value = server
        .post("/v1/quotes/9802/invoice-draft")
        .with_session(sessions::REP)
        .json(&json!({ "lines": [
            line("new:0", "SalesItemLineDetail", "Kitchen based @ 26.6 sqft", json!(500)),
            line("new:1", "DescriptionOnly", "40.9 lin ft Finished Edge - Flat", Value::Null),
            line("new:2", "DescriptionOnly", "40.9 lin ft Finished Edge - Eased", Value::Null),
            line("new:3", "DescriptionOnly", "Laser Template", Value::Null),
            line("new:4", "DescriptionOnly", "1 Sink - 60/40", Value::Null),
        ] }))
        .await
        .json();
    let texts: Vec<&str> = draft["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["description"].as_str().unwrap())
        .collect();
    assert_eq!(
        texts,
        [
            "Kitchen based @ 26.6 sqft",
            "Flat Edge",
            "40.9 lin ft Finished Edge - Eased",
            "1 Sink - 60/40"
        ]
    );
    assert_eq!(draft["lines"][0]["amount"], 500.0);
    assert_eq!(draft["removed"][0]["description"], "Laser Template");
    // Class is the quote's rep; the newest scanned invoice ("Rita") is 1 of 5.
    assert_eq!(draft["class_name"], "Rita Rep");
    assert_eq!(draft["bill_email_cc"], Value::Null);
    assert_eq!(draft["item_id"], "5");
    assert_eq!(draft["item_name"], "tile");
    assert_eq!(draft["lines"][0]["item_ref_value"], "5");
    let scanned: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM qbo_invoice_scans WHERE company_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(scanned, 1);
    server
        .post("/v1/quotes/424242/invoice-draft")
        .with_session(sessions::REP)
        .json(&json!({ "lines": [] }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn admins_read_recent_invoices_as_quickbooks_has_them(pool: MySqlPool) {
    let (server, _) = server(&pool, 10).await;
    let body: Value = server
        .get("/v1/qbo/invoices/recent?limit=5")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(body["invoices"][0]["ClassRef"]["name"], "Rita");
    assert_eq!(body["invoices"][0]["DocNumber"], "1001");
    server
        .get("/v1/qbo/invoices/recent")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
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
