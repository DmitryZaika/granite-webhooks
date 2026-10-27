use crate::common;

use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn a_layout_sees_free_slabs_of_the_stone_and_its_links(pool: MySqlPool) {
    let server = server(pool);
    let slabs = |query: &str| {
        let request = server
            .get(&format!("/v1/quotes/inventory-slabs?{query}"))
            .with_session(sessions::REP);
        async move { request.await.json::<Value>() }
    };
    let black = slabs("name=%20absolute%20black%20&type=GRANITE").await;
    assert_eq!(black["stone_url"], "https://img.example/absolute-black.jpg");
    let ids: Vec<i64> = black["slabs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [6000, 6001, 6002]);
    // The linked stone shows Absolute Black's free slabs.
    let linked = slabs("name=Linked%20Black%20Remnants").await;
    assert_eq!(linked["slabs"].as_array().unwrap().len(), 3);
    assert_eq!(
        slabs("name=Absolute%20Black&type=quartz").await["slabs"],
        json!([])
    );
    assert_eq!(
        slabs("name=").await,
        json!({ "stone_url": null, "slabs": [] })
    );
}

#[test_db::test(seed)]
async fn a_layout_never_offers_what_is_not_in_stock(pool: MySqlPool) {
    let server = server(pool);
    let slabs = |query: &str| {
        let request = server
            .get(&format!("/v1/quotes/inventory-slabs?{query}"))
            .with_session(sessions::REP);
        async move { request.await.json::<Value>() }
    };
    // No slabs and not regular stock: no slab and no stone photo to pick.
    assert_eq!(
        slabs("name=Sold%20Out%20Marble").await,
        json!({ "stone_url": null, "slabs": [] })
    );
    // Regular stock with no slabs: its photo may be used.
    assert_eq!(
        slabs("name=Regular%20Stock%20Quartzite").await["stone_url"],
        "https://img.example/regular.jpg"
    );
    // What is left of the sold CG-1 is offered, marked as a leftover.
    let gold = slabs("name=Calacatta%20Gold").await;
    assert_eq!(gold["slabs"][0]["id"], 6011);
    assert_eq!(gold["slabs"][0]["leftover"], true);
}

#[test_db::test(seed)]
async fn sold_slab_pictures_leave_unsigned_layouts(pool: MySqlPool) {
    sqlx::query(
        "UPDATE slab_inventory SET url = CONCAT('https://img.example/slab-', id, '.jpg')
          WHERE id IN (6000, 6003)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut doc = json!({
        "layouts": { "a": { "o": { "show": true, "slabs": [
            { "id": "s1", "width": 126, "height": 63, "image": "https://img.example/slab-6000.jpg" },
            { "id": "s2", "width": 126, "height": 63, "image": "https://img.example/slab-6003.jpg" },
            { "id": "s3", "width": 126, "height": 63, "image": "https://img.example/sold-out.jpg" },
            { "id": "s4", "width": 126, "height": 63, "image": "https://files.example/quotes/upload.jpg" },
            { "id": "s5", "width": 126, "height": 63, "image": null }
        ] } } }
    });
    let cleared = api::routes::quote_editor::drop_unavailable_slab_pictures(&pool, 100, &mut doc)
        .await
        .unwrap();
    assert_eq!(cleared, 2);
    let images: Vec<Value> = doc["layouts"]["a"]["o"]["slabs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|slab| slab["image"].clone())
        .collect();
    assert_eq!(
        images,
        [
            json!("https://img.example/slab-6000.jpg"),
            Value::Null,
            Value::Null,
            json!("https://files.example/quotes/upload.jpg"),
            Value::Null,
        ]
    );
}

/// Two quotes of company 100 (one the rep's, one deleted) and one of company 101.
async fn seed_quotes(pool: &MySqlPool) {
    sqlx::query(
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep,
                             company_id, salesperson_id, created_by, status, deleted_at)
         VALUES (9001, 1000, 'Kitchen', 'countertops', '2026-01-02 10:00:00', 'Rep', 100, 100, 100, 'converted', NULL),
                (9002, 1000, 'Bath', 'countertops', '2026-01-03 10:00:00', 'Admin', 100, 102, 102, 'draft', NULL),
                (9003, 1000, 'Gone', 'countertops', '2026-01-04 10:00:00', 'Rep', 100, 100, 100, 'draft', NOW()),
                (9004, 1000, 'Other', 'countertops', '2026-01-05 10:00:00', 'Rep', 101, NULL, 106, 'draft', NULL)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO quote_emails (quote_id, revision_number, token, recipients, subject, last_viewed_at)
         VALUES (9001, 1000, 't-9001', 'a@example.com', 'Quote', '2026-01-06 08:30:00')",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn the_quotes_list_shows_the_company_quotes_newest_first(pool: MySqlPool) {
    seed_quotes(&pool).await;
    let server = server(pool);
    let list = server
        .get("/v1/quotes")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    let quotes = list["quotes"].as_array().unwrap();
    let ids: Vec<i64> = quotes.iter().map(|q| q["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, [9002, 9001]);
    assert_eq!(quotes[0]["mine"], false);
    assert_eq!(quotes[1]["mine"], true);
    assert_eq!(quotes[1]["status"], "converted");
    assert_eq!(quotes[1]["created_date"], "2026-01-02T10:00:00Z");
    assert_eq!(quotes[1]["email_viewed_at"], "2026-01-06T08:30:00Z");
    assert!(list["default_show"] == "all" || list["default_show"] == "mine");
}

#[test_db::test(seed)]
async fn deleting_a_quote_hides_it_and_stays_in_the_company(pool: MySqlPool) {
    seed_quotes(&pool).await;
    let server = server(pool.clone());
    server
        .delete("/v1/quotes/9002")
        .with_session(sessions::REP)
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    let deleted: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT deleted_at FROM quotes WHERE id = 9002")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted.is_some());
    // Another company's quote, and one already deleted, are not found.
    for id in [9004, 9003] {
        server
            .delete(&format!("/v1/quotes/{id}"))
            .with_session(sessions::REP)
            .await
            .assert_status_not_found();
    }
}
