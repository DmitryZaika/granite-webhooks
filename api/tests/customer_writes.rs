mod common;

use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: company 100. Rita (100) is a sales rep (position 1), Mark (101) a
// manager, Dina (107) has no position. The default pipeline is 100 (first
// stage 100 "New"). Alice Lead (1000, Rita's) has alice@example.com and two
// more addresses; Bob Unassigned (1001) has no rep and no deal; Erin
// (1004) has the open deal 8601. Olga (106) is in company 101.

const RITA: &str = sessions::REP;
const DINA: &str = sessions::DEALER;

fn customer(name: &str, source: &str) -> Value {
    json!({ "name": name, "source": source, "emails": [], "phone": "317-555-0999" })
}

async fn open_deals(pool: &MySqlPool, customer_id: i64) -> Vec<(i64, Option<i32>, i32)> {
    sqlx::query_as(
        "SELECT CAST(id AS SIGNED), user_id, list_id FROM deals
         WHERE customer_id = ? AND deleted_at IS NULL AND is_won IS NULL ORDER BY id",
    )
    .bind(customer_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn trail(pool: &MySqlPool, customer_id: i64) -> Vec<(Option<String>, Option<String>)> {
    sqlx::query_as(
        "SELECT reassigned_by, reassigned_to FROM customers_history WHERE customer_id = ? ORDER BY id",
    )
    .bind(customer_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn create(server: &TestServer, session: &str, body: Value) -> axum_test::TestResponse {
    server
        .post("/v1/customers")
        .with_session(session)
        .json(&body)
        .await
}

#[test_db::test(seed)]
async fn a_sales_rep_creating_a_customer_gets_it_and_its_deal(pool: MySqlPool) {
    let server = server(pool.clone());
    let mut body = customer("New Lead", "leads");
    body["emails"] = json!(["New@Lead.test", "new@lead.test", "second@lead.test"]);
    let res = create(&server, RITA, body).await;
    res.assert_status(StatusCode::CREATED);
    let created: Value = res.json();
    let id = created["id"].as_i64().unwrap();
    let deals = open_deals(&pool, id).await;
    assert_eq!(deals.len(), 1);
    assert_eq!(created["deal_id"], deals[0].0);
    assert_eq!((deals[0].1, deals[0].2), (Some(100), 100));

    let (rep, assigned, created_by, primary): (
        Option<i32>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT c.sales_rep, c.assigned_date, c.created_by, ce.email FROM customers c
             LEFT JOIN customers_emails ce ON ce.id = c.email_id WHERE c.id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rep, Some(100));
    assert!(assigned.is_some());
    assert_eq!(created_by.as_deref(), Some("Rita Rep"));
    assert_eq!(primary.as_deref(), Some("New@Lead.test"));
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM customers_emails WHERE customer_id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 2);
    assert_eq!(
        trail(&pool, id).await,
        [(Some("Rita Rep".into()), Some("Rita Rep".into()))]
    );
}

#[test_db::test(seed)]
async fn walk_ins_and_non_reps_stay_unassigned(pool: MySqlPool) {
    let server = server(pool.clone());
    let walk_in: Value = create(&server, RITA, customer("Walk In", "check-in"))
        .await
        .json();
    assert_eq!(walk_in["deal_id"], Value::Null);
    assert!(
        open_deals(&pool, walk_in["id"].as_i64().unwrap())
            .await
            .is_empty()
    );
    let by_dina: Value = create(&server, DINA, customer("Dina's", "leads"))
        .await
        .json();
    assert_eq!(by_dina["deal_id"], Value::Null);

    // An explicit rep wins.
    let mut body = customer("Assigned", "check-in");
    body["sales_rep"] = json!(107);
    let assigned: Value = create(&server, RITA, body).await.json();
    let deals = open_deals(&pool, assigned["id"].as_i64().unwrap()).await;
    assert_eq!(deals[0].1, Some(107));
}

#[test_db::test(seed)]
async fn invalid_customers_are_refused(pool: MySqlPool) {
    let server = server(pool);
    let res = create(&server, RITA, customer("X", "made-up")).await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "Select a valid source.");
    create(&server, RITA, customer(" ", "leads"))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let mut foreign_rep = customer("X", "leads");
    foreign_rep["sales_rep"] = json!(106);
    create(&server, RITA, foreign_rep)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let mut taken = customer("X", "leads");
    taken["emails"] = json!(["ALICE@example.com"]);
    let res = create(&server, RITA, taken).await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        res.json::<Value>()["error"],
        "ALICE@example.com is already saved on Alice Lead"
    );
}

#[test_db::test(seed)]
async fn editing_contact_details_creates_no_deal(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .put("/v1/customers/1001")
        .with_session(RITA)
        .json(&json!({ "name": "Bob Renamed", "source": "check-in", "emails": ["bob@new.test"], "phone": "1" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(open_deals(&pool, 1001).await.is_empty());
    let (name, assigned): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT name, assigned_date FROM customers WHERE id = 1001")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(name, "Bob Renamed");
    assert!(assigned.is_none());
    assert!(trail(&pool, 1001).await.is_empty());
}

#[test_db::test(seed)]
async fn editing_addresses_replaces_them(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .put("/v1/customers/1000")
        .with_session(RITA)
        .json(&json!({
            "name": "Alice Lead", "source": "leads", "sales_rep": 100,
            "emails": ["alice.work@example.com", "alice.new@example.com"],
        }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let emails: Vec<String> = sqlx::query_scalar(
        "SELECT ce.email FROM customers_emails ce JOIN customers c ON c.id = ce.customer_id
         WHERE ce.customer_id = 1000 ORDER BY ce.id = c.email_id DESC, ce.id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(emails, ["alice.work@example.com", "alice.new@example.com"]);
}

#[test_db::test(seed)]
async fn assigning_in_the_edit_form_stamps_and_opens_one_deal(pool: MySqlPool) {
    let server = server(pool.clone());
    for _ in 0..2 {
        server
            .put("/v1/customers/1001")
            .with_session(RITA)
            .json(&json!({ "name": "Bob Unassigned", "source": "check-in", "sales_rep": 107 }))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    let deals = open_deals(&pool, 1001).await;
    assert_eq!(deals.len(), 1);
    assert_eq!(deals[0].1, Some(107));
    let assigned: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT assigned_date FROM customers WHERE id = 1001")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(assigned.is_some());
    assert_eq!(
        trail(&pool, 1001).await,
        [(Some("Rita Rep".into()), Some("Dina Dealer".into()))]
    );
}

#[test_db::test(seed)]
async fn the_sales_rep_route_assigns_reminds_and_keeps_the_first_date(pool: MySqlPool) {
    let server = server(pool.clone());
    let set = |rep: Value| {
        let server = &server;
        async move {
            server
                .put("/v1/customers/1001/sales-rep")
                .with_session(RITA)
                .json(&json!({ "sales_rep": rep }))
                .await
        }
    };
    set(json!(107)).await.assert_status(StatusCode::NO_CONTENT);
    let first: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT assigned_date FROM customers WHERE id = 1001")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(first.is_some());
    set(json!(107)).await.assert_status(StatusCode::NO_CONTENT);
    set(json!(100)).await.assert_status(StatusCode::NO_CONTENT);
    let deals = open_deals(&pool, 1001).await;
    assert_eq!(deals.len(), 1);
    assert_eq!(deals[0].1, Some(100));
    let again: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT assigned_date FROM customers WHERE id = 1001")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(again, first);
    let reminders: Vec<(i32, String)> = sqlx::query_as(
        "SELECT user_id, message FROM notifications WHERE customer_id = 1001 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        reminders,
        [
            (107, "Please text Bob Unassigned".into()),
            (107, "Please check the customer Bob Unassigned".into()),
            (100, "Please text Bob Unassigned".into()),
            (100, "Please check the customer Bob Unassigned".into()),
        ]
    );
    assert_eq!(
        trail(&pool, 1001).await,
        [
            (Some("Rita Rep".into()), Some("Dina Dealer".into())),
            (Some("Rita Rep".into()), Some("Rita Rep".into())),
        ]
    );

    set(Value::Null).await.assert_status(StatusCode::NO_CONTENT);
    let rep: Option<i32> = sqlx::query_scalar("SELECT sales_rep FROM customers WHERE id = 1001")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rep, None);
}

#[test_db::test(seed)]
async fn reassigning_a_lead_first_records_the_managers_assignment(pool: MySqlPool) {
    // Alice is a lead without a creator, assigned to Rita, with no trail.
    sqlx::query("UPDATE customers SET created_by = NULL WHERE id = 1000")
        .execute(&pool)
        .await
        .unwrap();
    server(pool.clone())
        .put("/v1/customers/1000/sales-rep")
        .with_session(RITA)
        .json(&json!({ "sales_rep": 107 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        trail(&pool, 1000).await,
        [
            (Some("sales manager".into()), Some("Rita Rep".into())),
            (Some("Rita Rep".into()), Some("Dina Dealer".into())),
        ]
    );
}

#[test_db::test(seed)]
async fn temperature_and_other_companies(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .put("/v1/customers/1001/temperature")
        .with_session(RITA)
        .json(&json!({ "temperature": "hot" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let temperature: Option<String> = sqlx::query_scalar(
        "SELECT CAST(customer_temperature AS CHAR) FROM customers WHERE id = 1001",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(temperature.as_deref(), Some("hot"));
    server
        .put("/v1/customers/1001/temperature")
        .with_session(RITA)
        .json(&json!({ "temperature": "lukewarm" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    for path in [
        "/v1/customers/2000/temperature",
        "/v1/customers/2000/sales-rep",
    ] {
        server
            .put(path)
            .with_session(RITA)
            .json(&json!({ "temperature": null, "sales_rep": null }))
            .await
            .assert_status_not_ok();
    }
    server
        .put("/v1/customers/2000")
        .with_session(RITA)
        .json(&customer("X", "leads"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn admins_and_sales_managers_delete_customers(pool: MySqlPool) {
    let server = server(pool.clone());
    // Reps may not.
    server
        .delete("/v1/customers/1007")
        .with_session(RITA)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // Mark manages sales in company 100.
    server
        .delete("/v1/customers/1007")
        .with_session(sessions::MANAGER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let gone: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM customers WHERE id = 1007")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(gone.is_some());

    // Erin has the live deal 8601: refused until the deals go too.
    let res = server
        .delete("/v1/customers/1004")
        .with_session(sessions::ADMIN)
        .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        res.json::<Value>()["error"],
        "Confirm removing associated deals"
    );
    server
        .delete("/v1/customers/1004?cascade_deals=true")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(open_deals(&pool, 1004).await.is_empty());

    server
        .delete("/v1/customers/2000")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
