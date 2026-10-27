use crate::common;

use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: sale 7010 has slab 6071 (Taj Mahal 3010, 125 x 64, Kitchen) whose
// unsold remnant is 6072 (60 x 30) and whose piece 6073 was sold to 7011.
// Free slabs of the stone: 6070, 6074. Sale 7012 has slab 6077 alone.

const ADMIN: &str = sessions::ADMIN;

async fn post(server: &TestServer, session: &str, path: &str, body: Value) -> StatusCode {
    server
        .post(path)
        .with_session(session)
        .json(&body)
        .await
        .status_code()
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct Slab {
    id: i64,
    sale_id: Option<i32>,
    length: Option<String>,
    width: Option<String>,
    cut: bool,
    room: Option<String>,
}

async fn slab(pool: &MySqlPool, id: i32) -> Option<Slab> {
    sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, sale_id, length, width, cut_date IS NOT NULL AS cut, room FROM slab_inventory WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn status(pool: &MySqlPool, sale: i32) -> String {
    sqlx::query_scalar("SELECT status FROM sales WHERE id = ?")
        .bind(sale)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn a_transaction_opens_with_its_slabs_and_sinks(pool: MySqlPool) {
    let server = server(pool);
    let body: Value = server
        .get("/v1/transactions/7010")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(body["sale"]["customer_name"], "Alice Lead");
    assert_eq!(body["slabs"][0]["id"], 6071);
    assert_eq!(body["slabs"][0]["child_count"], 2);
    assert_eq!(body["slabs"][0]["room"], "Kitchen");
    assert_eq!(body["sinks"].as_array().unwrap().len(), 3);
    server
        .get("/v1/transactions/7010")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn employees_mark_slabs_cut_and_uncut(pool: MySqlPool) {
    let server = server(pool.clone());
    let path = "/v1/transactions/7010/slabs/6071";
    assert_eq!(
        post(&server, sessions::REP, &format!("{path}/cut"), json!({})).await,
        StatusCode::NO_CONTENT
    );
    assert!(slab(&pool, 6071).await.unwrap().cut);
    // Without remnants the leftover stays.
    assert!(slab(&pool, 6072).await.is_some());
    assert_eq!(status(&pool, 7010).await, "cut");
    assert_eq!(
        post(&server, sessions::REP, &format!("{path}/uncut"), json!({})).await,
        StatusCode::NO_CONTENT
    );
    assert!(!slab(&pool, 6071).await.unwrap().cut);
    assert_eq!(status(&pool, 7010).await, "partially cut");
    // The remnant bookkeeping is for admins and the shop.
    assert_eq!(
        post(
            &server,
            sessions::REP,
            &format!("{path}/cut"),
            json!({ "with_remnants": true })
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(
            &server,
            sessions::OTHER_COMPANY_REP,
            &format!("{path}/cut"),
            json!({})
        )
        .await,
        StatusCode::NOT_FOUND
    );
}

#[test_db::test(seed)]
async fn admins_cut_and_uncut_with_remnants(pool: MySqlPool) {
    let server = server(pool.clone());
    let path = "/v1/transactions/7010/slabs/6071";
    assert_eq!(
        post(
            &server,
            ADMIN,
            &format!("{path}/cut"),
            json!({ "with_remnants": true })
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert!(slab(&pool, 6072).await.is_none());
    assert!(slab(&pool, 6073).await.is_some());
    // Uncut: the stone's full size goes back to the sold piece.
    sqlx::query("INSERT INTO slab_inventory (id, stone_id, bundle, parent_id, length, width) VALUES (6090, 3010, 'Slab 2', 6071, '10', '10')")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        post(
            &server,
            ADMIN,
            &format!("{path}/uncut"),
            json!({ "with_remnants": true })
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let piece = slab(&pool, 6073).await.unwrap();
    assert_eq!(
        (piece.length.as_deref(), piece.width.as_deref()),
        (Some("125"), Some("64"))
    );
    assert!(slab(&pool, 6090).await.is_none());
}

#[test_db::test(seed)]
async fn the_shop_records_cut_pieces(pool: MySqlPool) {
    let server = server(pool.clone());
    let path = "/v1/transactions/7012/slabs/6077/partial-cut";
    assert_eq!(
        post(
            &server,
            ADMIN,
            path,
            json!({ "length": 50.5, "width": 20, "add_another": true })
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let piece: (i64, Option<i32>, String, String) = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), sale_id, length, width FROM slab_inventory WHERE parent_id = 6077",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (piece.1, piece.2.as_str(), piece.3.as_str()),
        (Some(7012), "50.5", "20")
    );
    assert!(!slab(&pool, 6077).await.unwrap().cut);
    assert_eq!(
        post(
            &server,
            ADMIN,
            path,
            json!({ "length": 48, "width": 20, "replace_first": true })
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let resized: String = sqlx::query_scalar("SELECT length FROM slab_inventory WHERE id = ?")
        .bind(piece.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(resized, "48");
    assert!(slab(&pool, 6077).await.unwrap().cut);
    assert_eq!(
        post(&server, ADMIN, path, json!({ "length": 0, "width": 20 })).await,
        StatusCode::BAD_REQUEST
    );
}

#[test_db::test(seed)]
async fn admins_remove_replace_and_add_slabs(pool: MySqlPool) {
    let server = server(pool.clone());
    assert_eq!(
        post(
            &server,
            ADMIN,
            "/v1/transactions/7010/slabs/6071/replace",
            json!({ "new_slab_id": 6070 })
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let new = slab(&pool, 6070).await.unwrap();
    assert_eq!(
        (new.sale_id, new.room.as_deref()),
        (Some(7010), Some("Kitchen"))
    );
    let old = slab(&pool, 6071).await.unwrap();
    assert_eq!((old.sale_id, old.room), (None, None));
    assert!(slab(&pool, 6072).await.is_none());
    // A sold slab is not free.
    assert_eq!(
        post(
            &server,
            ADMIN,
            "/v1/transactions/7010/slabs/6070/replace",
            json!({ "new_slab_id": 6073 })
        )
        .await,
        StatusCode::BAD_REQUEST
    );

    assert_eq!(
        post(
            &server,
            ADMIN,
            "/v1/transactions/7010/slabs",
            json!({ "slab_id": 6074, "room": "Kitchen" })
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(slab(&pool, 6074).await.unwrap().sale_id, Some(7010));
    let res = server
        .put("/v1/transactions/7010/slabs/6074/square-feet")
        .with_session(ADMIN)
        .json(&json!({ "square_feet": 41.5 }))
        .await;
    res.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        post(
            &server,
            ADMIN,
            "/v1/transactions/7010/slabs/6074/remove",
            json!({})
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(slab(&pool, 6074).await.unwrap().sale_id, None);
    assert_eq!(
        post(
            &server,
            sessions::REP,
            "/v1/transactions/7010/slabs/6070/remove",
            json!({})
        )
        .await,
        StatusCode::FORBIDDEN
    );
}
