mod common;

use axum::http::StatusCode;
use common::{WithSession, ids, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn search(pool: MySqlPool, query: &str) -> Value {
    let res = server(pool)
        .get(&format!("/v1/stones/search{query}"))
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    res.json()
}

#[test_db::test(seed)]
async fn search_ranks_and_filters(pool: MySqlPool) {
    for (query, expected) in [
        // "Black" as a whole word ranks before a name ending with it.
        ("?name=black", vec![3007, 3000]),
        ("?name=absolute", vec![3000]),
        // Hidden only on request; deleted and other companies never.
        ("?name=granite", vec![3006, 3009]),
        ("?name=granite&include_hidden=true", vec![3004, 3006, 3009]),
        // Calacatta's only whole slab is sold: out unless sold out stones are asked for.
        ("?name=cala", vec![]),
        ("?name=cala&include_sold_out=true", vec![3001]),
        ("?name=marble", vec![]),
        ("?name=marble&include_sold_out=true", vec![3002]),
        // Regular stock without slabs is listed.
        ("?name=regular", vec![3003]),
        ("?name=", vec![]),
        ("?name=%25", vec![]),
        ("?name=a&limit=2", vec![3000, 3007]),
    ] {
        assert_eq!(ids(&search(pool.clone(), query).await), expected, "{query}");
    }
}

#[test_db::test(seed)]
async fn search_rows_have_the_list_shape(pool: MySqlPool) {
    let rows = search(pool.clone(), "?name=zebra").await;
    let list: Value = server(pool)
        .get("/v1/stones?type=quartz")
        .with_session(sessions::REP)
        .await
        .json();
    let zebra = list
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == 3008)
        .unwrap();
    assert_eq!(&rows[0], zebra);
}

#[test_db::test(seed)]
async fn search_needs_a_session_and_valid_params(pool: MySqlPool) {
    let server = server(pool);
    server
        .get("/v1/stones/search?name=a")
        .await
        .assert_status_unauthorized();
    server
        .get("/v1/stones/search?name=a&limit=lots")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

// ---- POST /v1/stones/quick

#[test_db::test(seed)]
async fn quick_add_creates_a_stone_with_a_leftover(pool: MySqlPool) {
    let res = server(pool.clone())
        .post("/v1/stones/quick")
        .with_session(sessions::REP)
        .json(&json!({
            "name": " Blue Bahia ", "type": "Granite", "retail_price": 120,
            "length": 60, "width": 30.5, "leftover": true
        }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let body: Value = res.json();
    assert_eq!(body["name"], "Blue Bahia");
    assert_eq!(body["type"], "Granite");
    let stone_id = body["id"].as_i64().unwrap();
    let slab_ids = body["slab_ids"].as_array().unwrap();
    assert_eq!(slab_ids.len(), 1);

    let (company, display, price): (i32, bool, Option<i32>) =
        sqlx::query_as("SELECT company_id, is_display, retail_price FROM stones WHERE id = ?")
            .bind(stone_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((company, display, price), (100, true, Some(120)));
    let (bundle, leftover, length, width): (String, bool, String, String) = sqlx::query_as(
        "SELECT bundle, is_leftover, length, width FROM slab_inventory WHERE id = ?",
    )
    .bind(slab_ids[0].as_i64().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(bundle.starts_with("LO-") && bundle.len() == 16, "{bundle}");
    assert_eq!(
        (leftover, length.as_str(), width.as_str()),
        (true, "60", "30.5")
    );
}

#[test_db::test(seed)]
async fn quick_add_with_bundles(pool: MySqlPool) {
    let body: Value = server(pool.clone())
        .post("/v1/stones/quick")
        .with_session(sessions::REP)
        .json(&json!({
            "name": "Viscont White", "type": "granite", "retail_price": 80,
            "leftover": false, "bundles": [" V-1 ", "V-2"]
        }))
        .await
        .json();
    let bundles: Vec<(String, bool)> = sqlx::query_as(
        "SELECT bundle, is_leftover FROM slab_inventory WHERE stone_id = ? ORDER BY id",
    )
    .bind(body["id"].as_i64().unwrap())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(bundles, vec![("V-1".into(), false), ("V-2".into(), false)]);
}

#[test_db::test(seed)]
async fn quick_add_validates(pool: MySqlPool) {
    let server = server(pool.clone());
    let base = json!({ "name": "X", "type": "granite", "retail_price": 10, "leftover": true });
    let with = |key: &str, value: Value| {
        let mut body = base.clone();
        body[key] = value;
        body
    };
    for (body, error) in [
        (with("name", json!("  ")), "Name is required"),
        (with("retail_price", json!(0)), "Price is required"),
        (
            with("length", json!(-1)),
            "Size must be a non-negative number",
        ),
        (with("type", json!("onyx")), "Select a valid stone type"),
        (with("leftover", json!(false)), "Bundle number is required"),
        (
            json!({ "name": "X", "type": "granite", "retail_price": 10, "leftover": false, "bundles": [""] }),
            "Bundle number is required",
        ),
        (with("company_id", json!(100)), "invalid body"),
    ] {
        let res = server
            .post("/v1/stones/quick")
            .with_session(sessions::REP)
            .json(&body)
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        let message = res.json::<Value>()["error"].as_str().unwrap().to_string();
        assert!(message.starts_with(error), "{body}: {message}");
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stones WHERE name = 'X'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    server
        .post("/v1/stones/quick")
        .json(&base)
        .await
        .assert_status_unauthorized();
}

// ---- GET /v1/slabs

#[test_db::test(seed)]
async fn slabs_by_id_with_their_stone(pool: MySqlPool) {
    let server = server(pool);
    let res = server
        .get("/v1/slabs?id=6074&id=6060&id=6011")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!([
            { "id": 6011, "bundle": "CG-1a", "is_leftover": false,
              "stone": { "id": 3001, "name": "Calacatta Gold", "type": "quartz" } },
            { "id": 6074, "bundle": "Slab 1", "is_leftover": false,
              "stone": { "id": 3010, "name": "Taj Mahal", "type": "quartzite" } }
        ])
    );
    let empty: Value = server
        .get("/v1/slabs")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(empty, json!([]));
    server
        .get("/v1/slabs?id=abc")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn stone_slabs_count_remnants(pool: MySqlPool) {
    let rows: Value = server(pool)
        .get("/v1/stones/3010/slabs")
        .with_session(sessions::REP)
        .await
        .json();
    let count = |id: i64| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap()["child_count"]
            .clone()
    };
    assert_eq!(count(6071), 2);
    assert_eq!(count(6074), 0);
}
