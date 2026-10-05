mod common;

use axum::http::StatusCode;
use common::{WithSession, ids, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

fn sorted(mut v: Vec<i64>) -> Vec<i64> {
    v.sort_unstable();
    v
}

fn row(rows: &Value, id: i64) -> &Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap_or_else(|| panic!("row {id} missing"))
}

// ---- GET /v1/customers ----------------------------------------------------

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn default_view_lists_valid_live_customers_of_own_company(pool: MySqlPool) {
    let rows: Value = server(pool)
        .get("/v1/customers")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        sorted(ids(&rows)),
        vec![
            1000,
            1001,
            1004,
            1005,
            1006,
            1008,
            1009,
            1010,
            1011,
            990_000_021
        ]
    );
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn customer_row_has_the_remix_shape(pool: MySqlPool) {
    let rows: Value = server(pool)
        .get("/v1/customers")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        row(&rows, 1000),
        &json!({
            "id": 1000,
            "name": "Alice Lead",
            "email": "alice@example.com",
            "phone": "317-555-0100",
            "phone_2": "317-555-0101",
            "address": "1 Main St",
            "sales_rep": 100,
            "created_date": "2026-01-15T10:00:00.000Z",
            "assigned_date": "2026-01-16T12:30:00.000Z",
            "sales_rep_name": "Rita Rep",
            "company_id": 100,
            "source": "leads",
            "invalid_lead": null,
            "company_name": null,
            "customerTemperature": "hot"
        })
    );
    // No company totals outside the companies view.
    assert!(row(&rows, 1000).get("revenue_generated").is_none());
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn joins_null_out_deleted_reps_and_foreign_primary_emails(pool: MySqlPool) {
    let rows: Value = server(pool)
        .get("/v1/customers")
        .with_session(sessions::REP)
        .await
        .json();
    let gina = row(&rows, 1006);
    assert_eq!(gina["sales_rep"], 105);
    assert_eq!(gina["sales_rep_name"], Value::Null);
    // Jack's email_id points at another customer's email row.
    assert_eq!(row(&rows, 1011)["email"], Value::Null);
    assert_eq!(row(&rows, 1001)["sales_rep"], Value::Null);
    assert_eq!(row(&rows, 1001)["assigned_date"], Value::Null);
    assert_eq!(row(&rows, 1010)["name"], "Zoë Ünicode-Łukasz");
    assert_eq!(row(&rows, 1004)["created_date"], "2026-03-10T23:59:59.000Z");
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn show_invalid_includes_invalid_leads(pool: MySqlPool) {
    let rows: Value = server(pool)
        .get("/v1/customers")
        .add_query_param("show_invalid", "1")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        sorted(ids(&rows)),
        vec![
            1000,
            1001,
            1002,
            1004,
            1005,
            1006,
            1007,
            1008,
            1009,
            1010,
            1011,
            990_000_021
        ]
    );
    assert_eq!(row(&rows, 1002)["invalid_lead"], "spam");
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn show_invalid_only_accepts_1(pool: MySqlPool) {
    let rows: Value = server(pool)
        .get("/v1/customers")
        .add_query_param("show_invalid", "true")
        .with_session(sessions::REP)
        .await
        .json();
    assert!(!ids(&rows).contains(&1002));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn sales_rep_filter(pool: MySqlPool) {
    let server = server(pool);
    let rows: Value = server
        .get("/v1/customers")
        .add_query_param("sales_rep", "100")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(sorted(ids(&rows)), vec![1000, 1008, 1011]);

    let rows: Value = server
        .get("/v1/customers")
        .add_query_param("sales_rep", "100")
        .add_query_param("show_invalid", "1")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(sorted(ids(&rows)), vec![1000, 1002, 1008, 1011]);

    // Empty value means "no filter", like the Remix `if (salesRepFilter)`.
    let rows: Value = server
        .get("/v1/customers")
        .add_query_param("sales_rep", "")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(ids(&rows).len(), 10);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn non_numeric_sales_rep_is_400(pool: MySqlPool) {
    let res = server(pool)
        .get("/v1/customers")
        .add_query_param("sales_rep", "abc")
        .with_session(sessions::REP)
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    res.assert_json(&json!({ "error": "invalid sales_rep: abc" }));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn unknown_view_falls_back_to_customers(pool: MySqlPool) {
    let rows: Value = server(pool)
        .get("/v1/customers")
        .add_query_param("view", "whatever")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(ids(&rows).len(), 10);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn companies_view_has_totals_and_ignores_invalid_filter(pool: MySqlPool) {
    let rows: Value = server(pool)
        .get("/v1/customers")
        .add_query_param("view", "companies")
        .with_session(sessions::REP)
        .await
        .json();
    // Hank is an invalid lead but companies always show; Ivy's '' name is excluded.
    assert_eq!(sorted(ids(&rows)), vec![1004, 1007]);
    assert_eq!(
        row(&rows, 1004),
        &json!({
            "id": 1004,
            "name": "Erin Builder",
            "email": "erin@builders.example",
            "phone": "317-555-0105",
            "phone_2": null,
            "address": "5 Oak Ave",
            "sales_rep": 101,
            "created_date": "2026-03-10T23:59:59.000Z",
            "assigned_date": null,
            "sales_rep_name": "Mark Manager",
            "company_id": 100,
            "source": "call-in",
            "invalid_lead": null,
            "company_name": "Erin Builders LLC",
            "customerTemperature": "medium",
            "revenue_generated": "3500.75",
            "projects_count": 3
        })
    );
    let hank = row(&rows, 1007);
    assert_eq!(hank["revenue_generated"], Value::Null);
    assert_eq!(hank["projects_count"], 0);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn companies_view_with_sales_rep(pool: MySqlPool) {
    let rows: Value = server(pool)
        .get("/v1/customers")
        .add_query_param("view", "companies")
        .add_query_param("sales_rep", "101")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(ids(&rows), vec![1004]);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn tenants_are_isolated(pool: MySqlPool) {
    let server = server(pool);
    let rows: Value = server
        .get("/v1/customers")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(ids(&rows), vec![2000]);

    let rows: Value = server
        .get("/v1/customers")
        .add_query_param("view", "companies")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(ids(&rows), vec![2000]);
    assert_eq!(rows[0]["revenue_generated"], "9999.99");
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn list_requires_a_session(pool: MySqlPool) {
    server(pool)
        .get("/v1/customers")
        .await
        .assert_status_unauthorized();
}

// ---- POST /v1/customers/emails/batch ---------------------------------------

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn emails_batch_orders_primary_first_then_by_creation(pool: MySqlPool) {
    let res = server(pool)
        .post("/v1/customers/emails/batch")
        .with_session(sessions::REP)
        .json(&json!({ "customer_ids": [1000, 1001, 1004, 1011, 1010] }))
        .await;
    res.assert_status_ok();
    res.assert_json(&json!({
        "1000": ["alice@example.com", "alice.work@example.com", "alice.old@example.com"],
        "1004": ["erin@builders.example"],
        "1010": ["zoe@example.com"],
        "1011": ["jack@example.com"]
    }));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn emails_batch_never_returns_other_companies(pool: MySqlPool) {
    server(pool)
        .post("/v1/customers/emails/batch")
        .with_session(sessions::REP)
        .json(&json!({ "customer_ids": [2000, 1004] }))
        .await
        .assert_json(&json!({ "1004": ["erin@builders.example"] }));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn emails_batch_edge_cases(pool: MySqlPool) {
    let server = server(pool);
    server
        .post("/v1/customers/emails/batch")
        .with_session(sessions::REP)
        .json(&json!({ "customer_ids": [] }))
        .await
        .assert_json(&json!({}));

    server
        .post("/v1/customers/emails/batch")
        .with_session(sessions::REP)
        .json(&json!({ "customer_ids": [1004, 1004, 1004] }))
        .await
        .assert_json(&json!({ "1004": ["erin@builders.example"] }));

    // Spans more than one internal chunk (1,000 ids each).
    let mut many: Vec<i64> = (1..=2_500).collect();
    many.push(1000);
    let res: Value = server
        .post("/v1/customers/emails/batch")
        .with_session(sessions::REP)
        .json(&json!({ "customer_ids": many }))
        .await
        .json();
    assert_eq!(res["1000"].as_array().unwrap().len(), 3);

    let too_many: Vec<i64> = (1..=20_001).collect();
    server
        .post("/v1/customers/emails/batch")
        .with_session(sessions::REP)
        .json(&json!({ "customer_ids": too_many }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    server
        .post("/v1/customers/emails/batch")
        .with_session(sessions::REP)
        .json(&json!({ "wrong": [] }))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn emails_batch_requires_a_session(pool: MySqlPool) {
    server(pool)
        .post("/v1/customers/emails/batch")
        .json(&json!({ "customer_ids": [1000] }))
        .await
        .assert_status_unauthorized();
}
