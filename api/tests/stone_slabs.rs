use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: stone 3010 Taj Mahal with slabs 6070-6077 and sales 7010-7012; stone
// 3007 draws from 3000 (stone_slab_links); 4000 is company 101's.

async fn get(pool: MySqlPool, path: &str) -> Value {
    let res = server(pool).get(path).with_session(sessions::REP).await;
    res.assert_status_ok();
    res.json()
}

fn slab_ids(rows: &Value) -> Vec<i64> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_i64().unwrap())
        .collect()
}

fn slab(rows: &Value, id: i64) -> &Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap_or_else(|| panic!("slab {id} missing"))
}

#[test_db::test(seed)]
async fn stone_has_every_field_and_its_colors(pool: MySqlPool) {
    let stone = get(pool, "/v1/stones/3008").await;
    assert_eq!(
        stone,
        json!({
            "id": 3008,
            "name": "Zebra Quartz",
            "type": "quartz",
            "url": "https://img.example/zebra.jpg",
            "supplier_id": 301,
            "is_display": true,
            "on_sale": false,
            "regular_stock": false,
            "length": 118.0,
            "width": 56.0,
            "retail_price": 95,
            "cost_per_sqft": 52,
            "level": 4,
            "finishing": "leathered",
            "samples_amount": 0,
            "samples_importance": null,
            "bundle_number": null,
            "bundle_location": null,
            "delivery_date": null,
            "created_date": "2026-02-18T09:30:00.000Z",
            "color_ids": [2, 15]
        })
    );
}

#[test_db::test(seed)]
async fn hidden_stones_are_returned_deleted_and_foreign_are_404(pool: MySqlPool) {
    let server = server(pool);
    let hidden = server
        .get("/v1/stones/3010")
        .with_session(sessions::REP)
        .await;
    hidden.assert_status_ok();
    assert_eq!(hidden.json::<Value>()["is_display"], false);
    assert_eq!(hidden.json::<Value>()["bundle_number"], "TM-7");
    assert_eq!(hidden.json::<Value>()["color_ids"], json!([]));
    for path in [
        "/v1/stones/3005",
        "/v1/stones/4000",
        "/v1/stones/999999",
        "/v1/stones/3005/slabs",
        "/v1/stones/4000/slabs",
    ] {
        let res = server.get(path).with_session(sessions::REP).await;
        res.assert_status(StatusCode::NOT_FOUND);
        res.assert_json(&json!({ "error": "not found" }));
    }
    // Company 101 sees its own stone.
    server
        .get("/v1/stones/4000/slabs")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn slabs_leave_out_cut_and_deleted(pool: MySqlPool) {
    let rows = get(pool, "/v1/stones/3010/slabs").await;
    assert_eq!(slab_ids(&rows), vec![6070, 6071, 6072, 6073, 6074, 6077]);
}

#[test_db::test(seed)]
async fn sold_slab_has_its_sale(pool: MySqlPool) {
    let rows = get(pool, "/v1/stones/3010/slabs").await;
    assert_eq!(
        slab(&rows, 6071),
        &json!({
            "id": 6071,
            "bundle": "Slab 2",
            "url": "https://img.example/tm-2.jpg",
            "length": "125",
            "width": "64",
            "parent_id": null,
            "is_leftover": false,
            // Two remnants were cut from it (6072, 6073).
            "child_count": 2,
            "source_stone": null,
            "sale_id": 7010,
            "sale": {
                "id": 7010,
                "sale_date": "2026-04-15T15:00:00.000Z",
                "customer_name": "Alice Lead",
                "seller_name": "Rita Rep",
                "notes": "Deliver after 5pm",
                "square_feet": "55.50",
                "slab_notes": "Edge polished",
                "room": "Kitchen",
                "slab_square_feet": "30.25",
                "sink_names": ["Composite Black", "Composite Black", "Bar Prep Sink"]
            },
            "parent_sale": null
        })
    );
}

#[test_db::test(seed)]
async fn remnants_carry_the_parent_sale(pool: MySqlPool) {
    let rows = get(pool, "/v1/stones/3010/slabs").await;
    let free_remnant = slab(&rows, 6072);
    assert_eq!(free_remnant["parent_id"], 6071);
    assert_eq!(free_remnant["sale_id"], Value::Null);
    assert_eq!(free_remnant["sale"], Value::Null);
    assert_eq!(
        free_remnant["parent_sale"],
        json!({ "id": 7010, "customer_name": "Alice Lead", "seller_name": "Rita Rep" })
    );
    let sold_remnant = slab(&rows, 6073);
    assert_eq!(
        sold_remnant["sale"],
        json!({
            "id": 7011,
            "sale_date": "2026-04-20T10:00:00.000Z",
            "customer_name": "Zoë Ünicode-Łukasz",
            "seller_name": "Mark Manager",
            "notes": null,
            "square_feet": "0.00",
            "slab_notes": null,
            "room": "Bath",
            "slab_square_feet": "0.00",
            // Sinks belong to the sale, not the slab: 7011 has none.
            "sink_names": []
        })
    );
    assert_eq!(sold_remnant["parent_sale"]["id"], 7010);
    // Free whole slabs have neither.
    let free = slab(&rows, 6074);
    assert_eq!(free["url"], Value::Null);
    assert_eq!(free["sale"], Value::Null);
    assert_eq!(free["parent_sale"], Value::Null);
}

#[test_db::test(seed)]
async fn sale_without_customer_or_seller(pool: MySqlPool) {
    let rows = get(pool, "/v1/stones/3010/slabs").await;
    let sale = &slab(&rows, 6077)["sale"];
    assert_eq!(sale["id"], 7012);
    assert_eq!(sale["customer_name"], Value::Null);
    assert_eq!(sale["seller_name"], Value::Null);
    assert_eq!(sale["square_feet"], Value::Null);
}

#[test_db::test(seed)]
async fn linked_stone_lists_the_source_slabs(pool: MySqlPool) {
    let rows = get(pool, "/v1/stones/3007/slabs").await;
    // 3000's slabs without the cut (6004) and deleted (6005) ones.
    assert_eq!(slab_ids(&rows), vec![6000, 6001, 6002, 6003]);
    for row in rows.as_array().unwrap() {
        assert_eq!(
            row["source_stone"],
            json!({ "id": 3000, "name": "Absolute Black" })
        );
    }
    let sold = slab(&rows, 6003);
    assert_eq!(sold["sale_id"], 7000);
    assert_eq!(sold["sale"]["customer_name"], "Erin Builder");
}

#[test_db::test(seed)]
async fn own_slabs_have_no_source_stone(pool: MySqlPool) {
    let rows = get(pool, "/v1/stones/3000/slabs").await;
    assert_eq!(slab_ids(&rows), vec![6000, 6001, 6002, 6003]);
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .all(|row| row["source_stone"].is_null())
    );
}

#[test_db::test(seed)]
async fn links_to_another_company_are_ignored(pool: MySqlPool) {
    sqlx::query("INSERT INTO stone_slab_links (stone_id, source_stone_id) VALUES (3010, 4000)")
        .execute(&pool)
        .await
        .unwrap();
    let rows = get(pool, "/v1/stones/3010/slabs").await;
    assert!(!slab_ids(&rows).contains(&6060));
}

#[test_db::test(seed)]
async fn require_an_employee_or_admin(pool: MySqlPool) {
    let server = server(pool);
    for path in ["/v1/stones/3010", "/v1/stones/3010/slabs"] {
        server.get(path).await.assert_status_unauthorized();
        server
            .get(path)
            .with_session(sessions::VIEWER)
            .await
            .assert_status(StatusCode::FORBIDDEN);
        server
            .get(path)
            .with_session(sessions::ADMIN)
            .await
            .assert_status_ok();
    }
}

// ---- PATCH /v1/slabs/{id}

async fn slab_size(pool: &MySqlPool, id: i64) -> (Option<String>, Option<String>) {
    sqlx::query_as("SELECT length, width FROM slab_inventory WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn resizes_a_slab(pool: MySqlPool) {
    let res = server(pool.clone())
        .patch("/v1/slabs/6074")
        .with_session(sessions::REP)
        .json(&json!({ "length": 130, "width": 65.5 }))
        .await;
    res.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        slab_size(&pool, 6074).await,
        (Some("130".into()), Some("65.5".into()))
    );
    // Admins too.
    server(pool.clone())
        .patch("/v1/slabs/6074")
        .with_session(sessions::ADMIN)
        .json(&json!({ "length": 1.25, "width": 2 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        slab_size(&pool, 6074).await,
        (Some("1.25".into()), Some("2".into()))
    );
}

#[test_db::test(seed)]
async fn resize_rejects_bad_sizes(pool: MySqlPool) {
    let server = server(pool.clone());
    for (body, error) in [
        (
            json!({ "length": 0, "width": 5 }),
            "length must be a positive number",
        ),
        (
            json!({ "length": 5, "width": -1 }),
            "width must be a positive number",
        ),
        (json!({ "length": "5", "width": 5 }), "invalid body"),
        (
            json!({ "length": 5 }),
            "length and width must be sent together",
        ),
        (json!({}), "nothing to change"),
        (json!({ "bundle": " " }), "Bundle name is required"),
        (
            json!({ "length": 5, "width": 5, "extra": 1 }),
            "invalid body",
        ),
    ] {
        let res = server
            .patch("/v1/slabs/6074")
            .with_session(sessions::REP)
            .json(&body)
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        let message = res.json::<Value>()["error"].as_str().unwrap().to_string();
        assert!(message.starts_with(error), "{body}: {message}");
    }
    assert_eq!(
        slab_size(&pool, 6074).await,
        (Some("124".into()), Some("62".into()))
    );
}

#[test_db::test(seed)]
async fn resize_is_scoped_to_the_company(pool: MySqlPool) {
    let server = server(pool.clone());
    for id in [6060, 6076, 999_999] {
        let res = server
            .patch(&format!("/v1/slabs/{id}"))
            .with_session(sessions::REP)
            .json(&json!({ "length": 1, "width": 1 }))
            .await;
        res.assert_status(StatusCode::NOT_FOUND);
    }
    assert_eq!(slab_size(&pool, 6060).await, (None, None));
    server
        .patch("/v1/slabs/6074")
        .json(&json!({ "length": 1, "width": 1 }))
        .await
        .assert_status_unauthorized();
    server
        .patch("/v1/slabs/6074")
        .with_session(sessions::VIEWER)
        .json(&json!({ "length": 1, "width": 1 }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

// ---- legacy "undefined" slab photo URLs read as null (QA audit 2026-10-14, B-015) --

#[test_db::test(seed)]
async fn legacy_undefined_slab_urls_read_as_null(pool: MySqlPool) {
    sqlx::query("UPDATE slab_inventory SET url = 'undefined' WHERE id = 6074")
        .execute(&pool)
        .await
        .unwrap();
    let stone: i32 = sqlx::query_scalar("SELECT stone_id FROM slab_inventory WHERE id = 6074")
        .fetch_one(&pool)
        .await
        .unwrap();
    let rows = get(pool.clone(), &format!("/v1/stones/{stone}/slabs")).await;
    assert_eq!(slab(&rows, 6074)["url"], Value::Null);
}
