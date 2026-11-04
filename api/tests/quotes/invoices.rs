use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: company 100's customers 1000 (Alice Lead) and 1004; 1003 is deleted.

fn body() -> Value {
    json!({
        "customer_id": 1000,
        "invoice_date": "2026-10-27",
        "due_date": "2026-11-10",
        "bill_to_name": " Alice Lead ",
        "bill_to_email": "alice@example.test",
        "project_address": "1 Main St, Indianapolis, IN",
        "lines": [
            { "description": "Kitchen @ 40 sqft", "qty": 40, "unit_price": 55.5 },
            { "description": "Eased edge" },
            { "description": "", "qty": null, "unit_price": null },
            { "description": "Discount", "unit_price": -100 }
        ],
        "tax": 147.0,
        "customer_memo": "Thank you!"
    })
}

async fn quote(pool: &MySqlPool, id: i32, customer_id: i32) {
    sqlx::query(
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep,
                             company_id, salesperson_id, created_by, status)
         VALUES (?, ?, 'Kitchen', 'countertops', NOW(), 'Rep', 100, 100, 100, 'converted')",
    )
    .bind(id)
    .bind(customer_id)
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn creates_numbered_drafts_with_totals(pool: MySqlPool) {
    let server = server(pool);
    let first = server
        .post("/v1/invoices")
        .with_session(sessions::REP)
        .json(&body())
        .await;
    first.assert_status(StatusCode::CREATED);
    let first: Value = first.json();
    assert_eq!(first["invoice_number"], 1);
    assert_eq!(first["status"], "draft");
    assert_eq!(first["bill_to_name"], "Alice Lead");
    assert_eq!(first["customer_name"], "Alice Lead");
    assert_eq!(first["due_date"], "2026-11-10");
    assert_eq!(first["sent_at"], Value::Null);
    assert_eq!(first["created_by_name"], "Rita Rep");
    // The blank line is dropped; amounts are worked out here.
    assert_eq!(
        first["lines"],
        json!([
            { "description": "Kitchen @ 40 sqft", "qty": 40.0, "unit_price": 55.5, "amount": 2220.0 },
            { "description": "Eased edge", "qty": null, "unit_price": null, "amount": null },
            { "description": "Discount", "qty": 1.0, "unit_price": -100.0, "amount": -100.0 }
        ])
    );
    assert_eq!(first["subtotal"], 2120.0);
    assert_eq!(first["tax"], 147.0);
    assert_eq!(first["total"], 2267.0);

    let second: Value = server
        .post("/v1/invoices")
        .with_session(sessions::REP)
        .json(&body())
        .await
        .json();
    assert_eq!(second["invoice_number"], 2);

    // A number given by hand is used; a taken one is refused.
    let mut chosen = body();
    chosen["invoice_number"] = json!(1001);
    let chosen: Value = server
        .post("/v1/invoices")
        .with_session(sessions::REP)
        .json(&chosen)
        .await
        .json();
    assert_eq!(chosen["invoice_number"], 1001);
    let mut taken = body();
    taken["invoice_number"] = json!(2);
    server
        .post("/v1/invoices")
        .with_session(sessions::REP)
        .json(&taken)
        .await
        .assert_status(StatusCode::CONFLICT);

    let list: Value = server
        .get("/v1/invoices")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(list["next_number"], 1002);
    let numbers: Vec<i64> = list["invoices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|invoice| invoice["invoice_number"].as_i64().unwrap())
        .collect();
    assert_eq!(numbers, vec![1001, 2, 1]);
    assert_eq!(list["invoices"][0]["total"], 2267.0);
}

#[test_db::test(seed)]
async fn refuses_bad_invoices(pool: MySqlPool) {
    let server = server(pool);
    let refused = |change: fn(&mut Value)| {
        let mut body = body();
        change(&mut body);
        body
    };
    for body in [
        refused(|b| b["bill_to_name"] = json!("  ")),
        refused(|b| b["bill_to_email"] = json!("not-an-email")),
        refused(|b| b["invoice_date"] = json!("10/27/2026")),
        refused(|b| b["due_date"] = json!("2026-10-01")),
        refused(|b| b["lines"] = json!([{ "description": "Only a note" }])),
        // Deleted, and another company's or unknown.
        refused(|b| b["customer_id"] = json!(1003)),
        refused(|b| b["quote_id"] = json!(424_242)),
    ] {
        server
            .post("/v1/invoices")
            .with_session(sessions::REP)
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
}

#[test_db::test(seed)]
async fn a_quote_bills_its_customer(pool: MySqlPool) {
    quote(&pool, 9851, 1000).await;
    let server = server(pool);
    let mut from_quote = body();
    from_quote["customer_id"] = Value::Null;
    from_quote["quote_id"] = json!(9851);
    let created = server
        .post("/v1/invoices")
        .with_session(sessions::REP)
        .json(&from_quote)
        .await;
    created.assert_status(StatusCode::CREATED);
    let created: Value = created.json();
    assert_eq!(created["customer_id"], 1000);
    assert_eq!(created["quote_id"], 9851);
    assert_eq!(created["quote_name"], "Kitchen");

    // Not this customer's quote.
    let mut mismatch = body();
    mismatch["customer_id"] = json!(1004);
    mismatch["quote_id"] = json!(9851);
    server
        .post("/v1/invoices")
        .with_session(sessions::REP)
        .json(&mismatch)
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    let by_quote: Value = server
        .get("/v1/invoices?quote_id=9851")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(by_quote["invoices"].as_array().unwrap().len(), 1);
}

#[test_db::test(seed)]
async fn edits_and_moves_through_statuses(pool: MySqlPool) {
    let server = server(pool);
    let created: Value = server
        .post("/v1/invoices")
        .with_session(sessions::REP)
        .json(&body())
        .await
        .json();
    let id = created["id"].as_i64().unwrap();

    let mut edited = body();
    edited["lines"] = json!([{ "description": "Vanity top", "qty": 2, "unit_price": 300 }]);
    edited["tax"] = json!(0);
    let saved = server
        .put(&format!("/v1/invoices/{id}"))
        .with_session(sessions::REP)
        .json(&edited)
        .await;
    saved.assert_status_ok();
    let saved: Value = saved.json();
    // The number is kept when the body leaves it out.
    assert_eq!(saved["invoice_number"], 1);
    assert_eq!(saved["total"], 600.0);
    // Saving the same invoice again still finds it.
    server
        .put(&format!("/v1/invoices/{id}"))
        .with_session(sessions::REP)
        .json(&edited)
        .await
        .assert_status_ok();

    let sent: Value = server
        .put(&format!("/v1/invoices/{id}/status"))
        .with_session(sessions::REP)
        .json(&json!({ "status": "sent" }))
        .await
        .json();
    assert_eq!(sent["status"], "sent");
    assert!(sent["sent_at"].is_string());
    assert_eq!(sent["paid_at"], Value::Null);

    let paid: Value = server
        .put(&format!("/v1/invoices/{id}/status"))
        .with_session(sessions::REP)
        .json(&json!({ "status": "paid" }))
        .await
        .json();
    assert_eq!(paid["status"], "paid");
    assert_eq!(paid["sent_at"], sent["sent_at"]);
    assert!(paid["paid_at"].is_string());

    let reopened: Value = server
        .put(&format!("/v1/invoices/{id}/status"))
        .with_session(sessions::REP)
        .json(&json!({ "status": "sent" }))
        .await
        .json();
    assert_eq!(reopened["paid_at"], Value::Null);

    server
        .put(&format!("/v1/invoices/{id}/status"))
        .with_session(sessions::REP)
        .json(&json!({ "status": "lost" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    let paid_list: Value = server
        .get("/v1/invoices?status=sent&search=alice")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(paid_list["invoices"].as_array().unwrap().len(), 1);
    let by_number: Value = server
        .get("/v1/invoices?search=%231")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(by_number["invoices"].as_array().unwrap().len(), 1);
}

#[test_db::test(seed)]
async fn other_companies_see_nothing(pool: MySqlPool) {
    let server = server(pool);
    let created: Value = server
        .post("/v1/invoices")
        .with_session(sessions::REP)
        .json(&body())
        .await
        .json();
    let id = created["id"].as_i64().unwrap();

    server
        .get(&format!("/v1/invoices/{id}"))
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let mut edited = body();
    edited["customer_id"] = Value::Null;
    server
        .put(&format!("/v1/invoices/{id}"))
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&edited)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put(&format!("/v1/invoices/{id}/status"))
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "status": "void" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let list: Value = server
        .get("/v1/invoices")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(list["invoices"], json!([]));
    assert_eq!(list["next_number"], 1);

    server
        .get("/v1/invoices")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}
