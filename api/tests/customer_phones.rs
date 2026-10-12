mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: Alice Lead (1000, 317-555-0100), Bob Unassigned (1001, 317-555-0102),
// Ivy Empty Strings (1008, empty phone); company 101's customer 2000.

fn ids(list: &Value) -> Vec<i64> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_i64().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn contacts_match_names_phones_and_suppliers(pool: MySqlPool) {
    sqlx::query("INSERT INTO suppliers (id, supplier_name, phone, company_id) VALUES (9100, 'Granite Supply', '(317) 555-0888', 100)")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(pool);
    let get = |term: &str| {
        let request = server
            .get(&format!("/v1/customers/sms-contacts?term={term}"))
            .with_session(sessions::REP);
        async move { request.await.json::<Value>() }
    };
    let alice = get("Alic").await;
    assert_eq!(ids(&alice["customers"]), [1000]);
    assert_eq!(alice["customers"][0]["phone"], "317-555-0100");
    assert_eq!(ids(&get("5550102").await["customers"]), [1001]);
    let supplier = get("granite").await;
    assert_eq!(supplier["suppliers"][0]["name"], "Granite Supply");
    assert_eq!(get("0888").await["suppliers"][0]["id"], 9100);
    assert_eq!(get("a").await, json!({ "customers": [], "suppliers": [] }));
    // Company 101's customer never shows.
    assert_eq!(
        ids(&get("Other%20Co").await["customers"]),
        Vec::<i64>::new()
    );
}

#[test_db::test(seed)]
async fn a_phone_fills_the_first_free_slot(pool: MySqlPool) {
    let server = server(pool.clone());
    let link = |id: i32, digits: &str| {
        let request = server
            .post(&format!("/v1/customers/{id}/phone"))
            .with_session(sessions::REP)
            .json(&json!({ "phone_digits": digits }));
        async move { request.await }
    };
    let res = link(1008, "13175550199").await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({ "customer_id": 1008, "customer_name": "Ivy Empty Strings" })
    );
    link(1000, "3175550198").await.assert_status_ok();
    let phones: Vec<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT phone, phone_2 FROM customers WHERE id IN (1000, 1008) ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(phones[0].1.as_deref(), Some("317-555-0198"));
    assert_eq!(phones[1].0.as_deref(), Some("317-555-0199"));

    link(1000, "12-34")
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    link(2000, "3175550198")
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn an_unknown_phone_becomes_a_customer(pool: MySqlPool) {
    let server = server(pool.clone());
    let res = server
        .post("/v1/customers/from-phone")
        .with_session(sessions::REP)
        .json(&json!({ "phone_digits": "3175550777", "name": "  New Person " }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let body: Value = res.json();
    assert_eq!(body["customer_name"], "New Person");
    let (company, phone): (i32, String) =
        sqlx::query_as("SELECT company_id, phone FROM customers WHERE id = ?")
            .bind(body["customer_id"].as_i64().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((company, phone.as_str()), (100, "317-555-0777"));
    let empty = server
        .post("/v1/customers/from-phone")
        .with_session(sessions::REP)
        .json(&json!({ "phone_digits": "3175550777", "name": " " }))
        .await;
    empty.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(empty.json::<Value>()["error"], "empty_name");
}

#[test_db::test(seed)]
async fn a_caller_is_found_by_either_phone(pool: MySqlPool) {
    let server = server(pool);
    let find = |phone: &str| {
        let request = server
            .get(&format!("/v1/customers/by-phone?phone={phone}"))
            .with_session(sessions::REP);
        async move { request.await.json::<Value>() }
    };
    assert_eq!(
        find("%2B13175550100").await,
        json!({ "customer_id": 1000, "customer_name": "Alice Lead" })
    );
    // Alice's second phone.
    assert_eq!(find("3175550101").await["customer_id"], 1000);
    assert_eq!(
        find("4635550200").await,
        json!({ "customer_id": null, "customer_name": null })
    );
    assert_eq!(find("").await["customer_id"], Value::Null);
}
