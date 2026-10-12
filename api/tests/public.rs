mod common;

use axum::http::StatusCode;
use common::{ids, server};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// The showroom answers without a session. Seed: company 100 (subdomain
// "acme") with stones 3000-3010, company 101 with 4000.

async fn get(pool: MySqlPool, path: &str) -> Value {
    let res = server(pool).get(path).await;
    res.assert_status_ok();
    res.json()
}

#[test_db::test(seed)]
async fn lists_displayed_stones_with_a_whole_slab_or_regular_stock(pool: MySqlPool) {
    let rows = get(pool.clone(), "/v1/public/companies/100/stones").await;
    // No Calacatta (remnants only), Sold Out Marble, hidden or deleted stones.
    assert_eq!(ids(&rows), vec![3000, 3007, 3006, 3009, 3003, 3008]);
    let absolute = &rows[0];
    assert_eq!(
        absolute,
        &json!({
            "id": 3000, "name": "Absolute Black", "type": "granite",
            "url": "https://img.example/absolute-black.jpg", "length": 126.5, "width": 63.0,
            "on_sale": true, "regular_stock": false, "retail_price": 75,
            "slab_price_per_sqft": null, "level": 2, "finishing": "polished",
            "bundle_number": "B-12", "created_date": "2026-01-10T08:00:00.000Z",
            "slabs": { "total": 4, "available": 3, "whole_total": 4, "whole_available": 3 }
        })
    );
    // Costs never leave the API.
    assert!(absolute.get("cost_per_sqft").is_none());
    assert_eq!(
        ids(&get(pool, "/v1/public/companies/101/stones").await),
        vec![4000]
    );
}

#[test_db::test(seed)]
async fn slab_price_only_for_stones_priced_by_slab(pool: MySqlPool) {
    sqlx::query("INSERT INTO slab_inventory (id, stone_id, bundle) VALUES (6090, 3001, 'CG-2')")
        .execute(&pool)
        .await
        .unwrap();
    let rows = get(pool, "/v1/public/companies/100/stones?type=quartz").await;
    let calacatta = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == 3001)
        .unwrap();
    assert_eq!(calacatta["retail_price"], 0);
    assert_eq!(calacatta["slab_price_per_sqft"], 55);
}

#[test_db::test(seed)]
async fn filters_and_search(pool: MySqlPool) {
    for (path, expected) in [
        ("/v1/public/companies/100/stones?type=quartz", vec![3008]),
        (
            "/v1/public/companies/100/stones?color_id=2",
            vec![3000, 3008],
        ),
        (
            "/v1/public/companies/100/stones?level=1&level=4",
            vec![3006, 3008],
        ),
        (
            "/v1/public/companies/100/stones?finishing=leathered",
            vec![3006, 3008],
        ),
        (
            "/v1/public/companies/100/stones/search?name=black",
            vec![3007, 3000],
        ),
        ("/v1/public/companies/100/stones/search?name=taj", vec![]),
        ("/v1/public/companies/100/stones/search?name=", vec![]),
    ] {
        assert_eq!(ids(&get(pool.clone(), path).await), expected, "{path}");
    }
    server(pool)
        .get("/v1/public/companies/100/stones?level=x")
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn stone_slabs_and_photos(pool: MySqlPool) {
    let stone = get(pool.clone(), "/v1/public/companies/100/stones/3002").await;
    // Sold out stones still open.
    assert_eq!(stone["name"], "Sold Out Marble");
    let slabs = get(pool.clone(), "/v1/public/companies/100/stones/3000/slabs").await;
    assert_eq!(
        slabs,
        json!([
            { "id": 6000, "bundle": "B-12-1", "url": null, "length": null, "width": null },
            { "id": 6001, "bundle": "B-12-2", "url": null, "length": null, "width": null },
            { "id": 6002, "bundle": "B-12-3", "url": null, "length": null, "width": null }
        ])
    );
    assert_eq!(
        ids(&get(pool.clone(), "/v1/public/companies/100/stones/3007/slabs").await),
        vec![6000, 6001, 6002]
    );
    assert_eq!(
        get(pool.clone(), "/v1/public/companies/100/stones/3000/images").await,
        json!([
            { "id": 100, "url": "https://img.example/installed-black-1.jpg" },
            { "id": 101, "url": "https://img.example/installed-black-2.jpg" }
        ])
    );
    let server = server(pool);
    for path in [
        "/v1/public/companies/100/stones/3004",
        "/v1/public/companies/100/stones/3004/slabs",
        "/v1/public/companies/100/stones/4000/slabs",
        "/v1/public/companies/101/stones/3000/images",
        "/v1/public/companies/100/stones/3005",
    ] {
        server.get(path).await.assert_status(StatusCode::NOT_FOUND);
    }
}

#[test_db::test(seed)]
async fn sinks_faucets_colors_and_subdomains(pool: MySqlPool) {
    let sinks = get(pool.clone(), "/v1/public/companies/100/sinks").await;
    // Displayed, in stock or regular stock; hidden Composite Black and
    // out-of-stock Ceramic Out are not shown.
    assert_eq!(ids(&sinks), vec![3107, 3105, 3101, 3100]);
    assert_eq!(
        sinks[3],
        json!({
            "id": 3100, "name": "Undermount 3219", "type": "stainless 18 gauge",
            "url": "https://img.example/undermount.jpg", "length": 32, "width": 19,
            "regular_stock": false, "available": 2, "retail_price": "199.00"
        })
    );
    assert!(sinks[3].get("cost").is_none());
    let faucets = get(
        pool.clone(),
        "/v1/public/companies/100/faucets?type=single%20handle",
    )
    .await;
    assert_eq!(ids(&faucets), vec![3200]);
    assert_eq!(faucets[0]["length"], Value::Null);
    assert_eq!(
        get(pool.clone(), "/v1/public/colors")
            .await
            .as_array()
            .unwrap()
            .len(),
        16
    );
    assert_eq!(
        get(pool.clone(), "/v1/public/companies/by-subdomain/acme").await,
        json!({ "id": 100 })
    );
    server(pool)
        .get("/v1/public/companies/by-subdomain/nope")
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn a_vcard_is_built_from_the_query(pool: MySqlPool) {
    let server = server(pool);
    let res = server
        .get("/v1/public/vcard")
        .add_query_param("name", "Alice Lead")
        .add_query_param("phone", "317-555-0100")
        .add_query_param("company", "null")
        .add_query_param("filename", "Alice Lead")
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.header("content-disposition"),
        "attachment; filename=\"Alice_Lead.vcf\""
    );
    assert_eq!(
        res.text(),
        "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Alice Lead\r\nN:Alice Lead;;;;\r\nTEL:317-555-0100\r\nEND:VCARD\r\n"
    );
    server
        .get("/v1/public/vcard")
        .add_query_param("name", " ")
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);
}
