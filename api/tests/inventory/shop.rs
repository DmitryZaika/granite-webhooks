use crate::common;

use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn cut_stats_count_fully_cut_rooms_by_day_and_material(pool: MySqlPool) {
    // Sale 7010 (company 100): the kitchen has an uncut slab (left out);
    // both bath slabs are cut, a day apart.
    sqlx::query(
        "INSERT INTO slab_inventory (id, stone_id, bundle, sale_id, room, cut_date, square_feet, length, width)
         VALUES (9000, 3000, 'X-0', 7010, 'test kitchen', '2026-04-01 10:00:00', 30, NULL, NULL),
                (9001, 3000, 'X-1', 7010, 'test kitchen', NULL, 10, NULL, NULL),
                (9002, 3001, 'X-2', 7010, 'test bath', '2026-04-01 12:00:00', NULL, 144, 20),
                (9003, 3000, 'X-3', 7010, 'test bath', '2026-04-02 09:00:00', 12.5, NULL, NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let body: Value = server
        .get("/v1/shop/cut-stats?start=2026-03-30&end=2026-04-05")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        body,
        json!({
            "start": "2026-03-30",
            "end": "2026-04-05",
            "days": [
                { "day": "2026-04-01", "total": 20.0,
                  "materials": [{ "stone_type": "quartz", "sqft": 20.0 }] },
                { "day": "2026-04-02", "total": 12.5,
                  "materials": [{ "stone_type": "granite", "sqft": 12.5 }] },
            ],
            "materials": [
                { "stone_type": "quartz", "sqft": 20.0 },
                { "stone_type": "granite", "sqft": 12.5 },
            ],
            "total_sqft": 32.5,
        })
    );
    server
        .get("/v1/shop/cut-stats?start=2026-04-05&end=2026-04-01")
        .with_session(sessions::REP)
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);
}
