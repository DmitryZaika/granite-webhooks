mod common;

use axum::http::StatusCode;
use common::{WithSession, ids, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: stones 3000-3009 of company 100, 4000 of company 101, slabs 6000+.

async fn list(pool: MySqlPool, query: &str) -> Value {
    let res = server(pool)
        .get(&format!("/v1/stones{query}"))
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    res.json()
}

fn row(rows: &Value, id: i64) -> &Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap_or_else(|| panic!("stone {id} missing"))
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn lists_displayed_in_stock_stones_sorted_by_name(pool: MySqlPool) {
    let rows = list(pool, "").await;
    // Absolute Black, Calacatta Gold, Linked Black Remnants, No Image Granite,
    // Onboarding Sample Granite, Regular Stock Quartzite, Zebra Quartz.
    assert_eq!(ids(&rows), vec![3000, 3001, 3007, 3006, 3009, 3003, 3008]);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn stone_has_every_field(pool: MySqlPool) {
    let rows = list(pool, "").await;
    assert_eq!(
        row(&rows, 3000),
        &json!({
            "id": 3000,
            "name": "Absolute Black",
            "type": "granite",
            "url": "https://img.example/absolute-black.jpg",
            "supplier_id": 300,
            "is_display": true,
            "on_sale": true,
            "regular_stock": false,
            "length": 126.5,
            "width": 63.0,
            "retail_price": 75,
            "cost_per_sqft": 40,
            "level": 2,
            "finishing": "polished",
            "samples_amount": 3,
            "samples_importance": 1,
            "bundle_number": "B-12",
            "bundle_location": "Rack 3",
            "delivery_date": "2026-05-01",
            "created_date": "2026-01-10T08:00:00.000Z",
            // 6 slabs: 3 free, 1 sold, 1 cut, 1 deleted.
            "slabs": { "total": 4, "available": 3, "whole_total": 4, "whole_available": 3 }
        })
    );
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn slab_counts_split_remnants_links_and_regular_stock(pool: MySqlPool) {
    let rows = list(pool, "").await;
    // Whole slab sold, the remnant cut from it free.
    assert_eq!(
        row(&rows, 3001)["slabs"],
        json!({ "total": 2, "available": 1, "whole_total": 1, "whole_available": 0 })
    );
    // No own slabs, but linked to Absolute Black.
    assert_eq!(row(&rows, 3007)["slabs"], row(&rows, 3000)["slabs"]);
    // Regular stock is listed with no slabs at all.
    let regular = row(&rows, 3003);
    assert_eq!(regular["regular_stock"], true);
    assert_eq!(
        regular["slabs"],
        json!({ "total": 0, "available": 0, "whole_total": 0, "whole_available": 0 })
    );
    assert_eq!(regular["level"], Value::Null);
    assert_eq!(regular["finishing"], Value::Null);
    assert_eq!(regular["delivery_date"], Value::Null);
    assert_eq!(row(&rows, 3006)["url"], Value::Null);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn include_sold_out_adds_stones_without_free_slabs(pool: MySqlPool) {
    let rows = list(pool, "?include_sold_out=true").await;
    assert_eq!(
        ids(&rows),
        vec![3000, 3001, 3007, 3006, 3009, 3003, 3002, 3008]
    );
    let sold_out = row(&rows, 3002);
    assert_eq!(sold_out["bundle_number"], "0");
    assert_eq!(sold_out["bundle_location"], "");
    assert_eq!(sold_out["length"], Value::Null);
    assert_eq!(sold_out["slabs"]["available"], 0);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn never_returns_hidden_deleted_or_foreign_stones(pool: MySqlPool) {
    let rows = list(pool, "?include_sold_out=true").await;
    let ids = ids(&rows);
    for hidden in [3004, 3005, 4000] {
        assert!(!ids.contains(&hidden), "stone {hidden} must not be listed");
    }
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn filters(pool: MySqlPool) {
    let server = server(pool);
    let cases: &[(&str, &[i64])] = &[
        ("?type=quartz", &[3001, 3008]),
        (
            "?type=granite&type=marble&include_sold_out=true",
            &[3000, 3007, 3006, 3009, 3002],
        ),
        ("?supplier_id=301", &[3001, 3008]),
        ("?color_id=2", &[3000, 3008]),
        ("?color_id=2&color_id=15", &[3000, 3001, 3008]),
        ("?level=2", &[3000, 3007, 3009]),
        ("?level=4&level=5", &[3001, 3008]),
        ("?finishing=leathered", &[3006, 3008]),
        ("?type=quartz&finishing=honed&finishing=polished", &[3001]),
        ("?type=granite&supplier_id=301", &[]),
        ("?type=onyx", &[]),
        // Empty lists and false are the defaults.
        (
            "?include_sold_out=false",
            &[3000, 3001, 3007, 3006, 3009, 3003, 3008],
        ),
    ];
    for (query, expected) in cases {
        let res = server
            .get(&format!("/v1/stones{query}"))
            .with_session(sessions::REP)
            .await;
        res.assert_status_ok();
        assert_eq!(ids(&res.json()), *expected, "{query}");
    }
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn malformed_filters_are_400(pool: MySqlPool) {
    let server = server(pool);
    for query in ["?supplier_id=abc", "?level=high", "?include_sold_out=yes"] {
        let res = server
            .get(&format!("/v1/stones{query}"))
            .with_session(sessions::REP)
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        let error = res.json::<Value>()["error"].as_str().unwrap().to_string();
        assert!(
            error.starts_with("invalid query string"),
            "{query}: {error}"
        );
    }
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn tenants_are_isolated(pool: MySqlPool) {
    let rows: Value = server(pool)
        .get("/v1/stones?include_sold_out=true")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(ids(&rows), vec![4000]);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn requires_an_employee_or_admin(pool: MySqlPool) {
    let server = server(pool);
    server.get("/v1/stones").await.assert_status_unauthorized();
    server
        .get("/v1/stones")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/stones")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
}
