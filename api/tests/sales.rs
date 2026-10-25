use crate::common;

use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: stone 3010 Taj Mahal with slabs 6070 (free), 6071 (sale 7010, Alice,
// sold by Rita; remnants 6072 free and 6073 sold on 7011), 6074 (free, no
// image), 6077 (sale 7012, no customer). Free sink units: 6100/6101 of 3100,
// 6104 of 3102, 6106 of 3105, 6107 of 3107; 3101 has none. Free faucets:
// 6200/6201 of 3200, 6204 of 3202. Customer 1001 has no sales rep.

const ROOM_A: &str = "0192f1a0-0000-7000-8000-00000000000a";
const ROOM_B: &str = "0192f1a0-0000-7000-8000-00000000000b";

fn room(room_id: &str, slabs: Value, square_feet: f64) -> Value {
    json!({
        "room_id": room_id,
        "room": "kitchen",
        "backsplash": "no",
        "square_feet": square_feet,
        "retail_price": 50,
        "tear_out": "no",
        "stove": "f/s",
        "waterfall": "no",
        "corbels": 0,
        "seam": "standard",
        "ten_year_sealer": false,
        "extras": { "edge_price": { "edge_type": "flat", "price": 0 }, "tear_out_price": 0 },
        "slabs": slabs,
        "sinks": [],
        "faucets": []
    })
}

fn sale(customer_id: i64, rooms: Value) -> Value {
    json!({
        "customer_id": customer_id,
        "notes": "Back door",
        "price": 1500,
        "extras": [],
        "rooms": rooms
    })
}

async fn create(server: &TestServer, body: &Value) -> i64 {
    let res = server
        .post("/v1/sales")
        .with_session(sessions::REP)
        .json(body)
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json::<Value>()["id"].as_i64().unwrap()
}

async fn expect_400(server: &TestServer, method: &str, path: &str, body: &Value, error: &str) {
    let req = match method {
        "PUT" => server.put(path),
        _ => server.post(path),
    };
    let res = req.with_session(sessions::REP).json(body).await;
    res.assert_status(StatusCode::BAD_REQUEST);
    let message = res.json::<Value>()["error"].as_str().unwrap().to_string();
    assert!(message.starts_with(error), "{body}: {message}");
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct SlabState {
    id: i64,
    sale_id: Option<i32>,
    parent_id: Option<i64>,
    room: Option<String>,
    room_id: Option<String>,
    square_feet: Option<String>,
    price: Option<String>,
    bundle: Option<String>,
    length: Option<String>,
}

async fn slab(pool: &MySqlPool, id: i64) -> Option<SlabState> {
    sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, sale_id, CAST(parent_id AS SIGNED) AS parent_id,
            room, BIN_TO_UUID(room_uuid) AS room_id, CAST(square_feet AS CHAR) AS square_feet,
            CAST(price AS CHAR) AS price, bundle, length
         FROM slab_inventory WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn children(pool: &MySqlPool, parent: i64) -> Vec<SlabState> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM slab_inventory WHERE parent_id = ? ORDER BY id",
    )
    .bind(parent)
    .fetch_all(pool)
    .await
    .unwrap();
    let mut rows = Vec::new();
    for id in ids {
        rows.push(slab(pool, id).await.unwrap());
    }
    rows
}

async fn unit_slab(pool: &MySqlPool, table: &str, id: i64) -> (Option<i64>, Option<String>) {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(slab_id AS SIGNED), CAST(price AS CHAR) FROM {table} WHERE id = ?"
    )))
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn sale_count(pool: &MySqlPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM sales")
        .fetch_one(pool)
        .await
        .unwrap()
}

// ---- POST /v1/sales

#[test_db::test(seed)]
async fn sells_part_of_a_slab_and_leaves_a_remnant(pool: MySqlPool) {
    let server = server(pool.clone());
    let id = create(
        &server,
        &sale(
            1000,
            json!([room(
                ROOM_A,
                json!([{ "id": 6074, "is_full": false }]),
                30.0
            )]),
        ),
    )
    .await;

    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct SaleRow {
        customer_id: i32,
        seller_id: Option<i32>,
        company_id: Option<i32>,
        status: String,
        notes: Option<String>,
        square_feet: Option<String>,
        price: Option<String>,
        project_address: String,
        extras: Option<String>,
        has_date: bool,
    }
    let row: SaleRow = sqlx::query_as(
        "SELECT customer_id, seller_id, company_id, status, notes,
            CAST(square_feet AS CHAR) AS square_feet, CAST(price AS CHAR) AS price,
            project_address, CAST(extras AS CHAR) AS extras, sale_date IS NOT NULL AS has_date
         FROM sales WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        SaleRow {
            customer_id: 1000,
            seller_id: Some(100),
            company_id: Some(100),
            status: "pending".into(),
            notes: Some("Back door".into()),
            square_feet: Some("30.00".into()),
            price: Some("1500.00".into()),
            // Customer's address when none is given.
            project_address: "1 Main St".into(),
            extras: Some("[]".into()),
            has_date: true,
        }
    );

    let sold = slab(&pool, 6074).await.unwrap();
    assert_eq!(sold.sale_id, Some(i32::try_from(id).unwrap()));
    assert_eq!(sold.room.as_deref(), Some("kitchen"));
    assert_eq!(sold.room_id.as_deref(), Some(ROOM_A));
    assert_eq!(sold.square_feet.as_deref(), Some("30.00"));
    assert_eq!(sold.price.as_deref(), Some("50.00"));
    let remnants = children(&pool, 6074).await;
    assert_eq!(remnants.len(), 1);
    assert_eq!(remnants[0].sale_id, None);
    assert_eq!(remnants[0].bundle.as_deref(), Some("Slab 1"));
    assert_eq!(remnants[0].length.as_deref(), Some("124"));
    assert_eq!(remnants[0].room, None);

    // Alice already had a rep: nothing logged.
    let history: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM customers_history WHERE customer_id = 1000")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(history, 0);
}

#[test_db::test(seed)]
async fn a_customer_without_a_rep_gets_the_seller(pool: MySqlPool) {
    let server = server(pool.clone());
    let mut body = sale(
        1001,
        json!([room(ROOM_A, json!([{ "id": 6070, "is_full": true }]), 10.0)]),
    );
    body["seller_id"] = json!(101);
    body["project_address"] = json!("9 Job Site Rd");
    let id = create(&server, &body).await;

    let (rep, assigned): (Option<i32>, bool) = sqlx::query_as(
        "SELECT sales_rep, assigned_date IS NOT NULL FROM customers WHERE id = 1001",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((rep, assigned), (Some(101), true));
    let history: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT reassigned_by, reassigned_to FROM customers_history WHERE customer_id = 1001",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        history,
        vec![("Rita Rep".into(), Some("Mark Manager".into()))]
    );
    let (seller, address): (Option<i32>, String) =
        sqlx::query_as("SELECT seller_id, project_address FROM sales WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((seller, address.as_str()), (Some(101), "9 Job Site Rd"));
}

#[test_db::test(seed)]
async fn whole_slab_with_sink_and_faucet(pool: MySqlPool) {
    let server = server(pool.clone());
    let mut kitchen = room(ROOM_A, json!([{ "id": 6070, "is_full": true }]), 45.0);
    kitchen["sinks"] = json!([{ "type_id": 3100, "price": 199 }]);
    kitchen["faucets"] = json!([{ "type_id": 3200, "price": 129.99 }]);
    create(&server, &sale(1000, json!([kitchen]))).await;

    assert!(children(&pool, 6070).await.is_empty());
    // The lowest free unit of each model.
    assert_eq!(
        unit_slab(&pool, "sinks", 6100).await,
        (Some(6070), Some("199.00".into()))
    );
    assert_eq!(unit_slab(&pool, "sinks", 6101).await, (None, None));
    assert_eq!(
        unit_slab(&pool, "faucets", 6200).await,
        (Some(6070), Some("129.99".into()))
    );
}

#[test_db::test(seed)]
async fn a_slab_in_two_rooms_is_cut_for_the_second(pool: MySqlPool) {
    let server = server(pool.clone());
    let mut second = room(ROOM_B, json!([{ "id": 6072, "is_full": false }]), 6.0);
    second["room"] = json!("island");
    second["sinks"] = json!([{ "type_id": 3105, "price": 329 }]);
    let id = create(
        &server,
        &sale(
            1000,
            json!([
                room(ROOM_A, json!([{ "id": 6072, "is_full": false }]), 8.0),
                second
            ]),
        ),
    )
    .await;
    let sale_id = Some(i32::try_from(id).unwrap());

    assert_eq!(slab(&pool, 6072).await.unwrap().sale_id, sale_id);
    let pieces = children(&pool, 6072).await;
    // The piece for room 2 (sold), then the remnant room 1 left (free).
    assert_eq!(pieces.len(), 2);
    let piece = &pieces[0];
    assert_eq!(piece.sale_id, sale_id);
    assert_eq!(piece.room.as_deref(), Some("island"));
    assert_eq!(piece.room_id.as_deref(), Some(ROOM_B));
    assert_eq!(piece.length.as_deref(), Some("60"));
    assert_eq!(pieces[1].sale_id, None);
    // Room 2 was partial too: its piece left a remnant.
    let piece_remnants = children(&pool, piece.id).await;
    assert_eq!(piece_remnants.len(), 1);
    assert_eq!(piece_remnants[0].sale_id, None);
    // Room 2's sink sits on room 2's slab.
    assert_eq!(unit_slab(&pool, "sinks", 6106).await.0, Some(piece.id));
}

#[test_db::test(seed)]
async fn invalid_sales_are_rejected_before_any_write(pool: MySqlPool) {
    let server = server(pool.clone());
    let free = || json!([{ "id": 6074, "is_full": false }]);
    let before = sale_count(&pool).await;
    let cases: Vec<(Value, &str)> = vec![
        (sale(1000, json!([])), "at least one room is required"),
        (
            sale(1000, json!([room(ROOM_A, json!([]), 5.0)])),
            "At least one slab is required in each room",
        ),
        (
            sale(1000, json!([room(ROOM_A, free(), 0.0)])),
            "Square feet is required",
        ),
        (
            sale(1000, json!([room("not-a-uuid", free(), 5.0)])),
            "invalid room_id",
        ),
        (
            sale(999_999, json!([room(ROOM_A, free(), 5.0)])),
            "customer 999999 not found",
        ),
        // Another company's customer, a deleted customer.
        (
            sale(2000, json!([room(ROOM_A, free(), 5.0)])),
            "customer 2000 not found",
        ),
        (
            sale(1003, json!([room(ROOM_A, free(), 5.0)])),
            "customer 1003 not found",
        ),
        // Another company's slab, a deleted slab, a sold slab.
        (
            sale(
                1000,
                json!([room(ROOM_A, json!([{ "id": 6060, "is_full": true }]), 5.0)]),
            ),
            "slab 6060 not found",
        ),
        (
            sale(
                1000,
                json!([room(ROOM_A, json!([{ "id": 6076, "is_full": true }]), 5.0)]),
            ),
            "slab 6076 not found",
        ),
        (
            sale(
                1000,
                json!([room(ROOM_A, json!([{ "id": 6071, "is_full": true }]), 5.0)]),
            ),
            "slab 6071 is already sold (sale 7010)",
        ),
    ];
    for (body, error) in &cases {
        expect_400(&server, "POST", "/v1/sales", body, error).await;
    }

    let mut foreign_sink = room(ROOM_A, free(), 5.0);
    foreign_sink["sinks"] = json!([{ "type_id": 3106, "price": 1 }]);
    expect_400(
        &server,
        "POST",
        "/v1/sales",
        &sale(1000, json!([foreign_sink])),
        "sink model 3106 not found",
    )
    .await;

    let mut seller = sale(1000, json!([room(ROOM_A, free(), 5.0)]));
    seller["seller_id"] = json!(105);
    expect_400(
        &server,
        "POST",
        "/v1/sales",
        &seller,
        "seller 105 not found",
    )
    .await;

    let mut malformed = sale(1000, json!([room(ROOM_A, free(), 5.0)]));
    malformed["unexpected"] = json!(1);
    expect_400(&server, "POST", "/v1/sales", &malformed, "invalid body").await;

    assert_eq!(sale_count(&pool).await, before);
    assert_eq!(slab(&pool, 6074).await.unwrap().sale_id, None);
}

#[test_db::test(seed)]
async fn no_free_unit_rolls_the_whole_sale_back(pool: MySqlPool) {
    let server = server(pool.clone());
    let before = sale_count(&pool).await;
    // Farmhouse 33 (3101) has no units at all.
    let mut kitchen = room(ROOM_A, json!([{ "id": 6074, "is_full": false }]), 5.0);
    kitchen["sinks"] = json!([{ "type_id": 3101, "price": 0 }]);
    expect_400(
        &server,
        "POST",
        "/v1/sales",
        &sale(1001, json!([kitchen])),
        "No available sinks of type 3101. Cannot complete the sale.",
    )
    .await;
    assert_eq!(sale_count(&pool).await, before);
    assert_eq!(slab(&pool, 6074).await.unwrap().sale_id, None);
    assert!(children(&pool, 6074).await.is_empty());
    let rep: Option<i32> = sqlx::query_scalar("SELECT sales_rep FROM customers WHERE id = 1001")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rep, None);

    // Two units of a model with one free unit: also all or nothing.
    let mut two = room(ROOM_A, json!([{ "id": 6074, "is_full": false }]), 5.0);
    two["faucets"] = json!([{ "type_id": 3202, "price": 1 }, { "type_id": 3202, "price": 1 }]);
    expect_400(
        &server,
        "POST",
        "/v1/sales",
        &sale(1000, json!([two])),
        "No available faucets of type 3202",
    )
    .await;
    assert_eq!(unit_slab(&pool, "faucets", 6204).await, (None, None));
}

// ---- GET /v1/sales/{id}

#[test_db::test(seed)]
async fn get_returns_the_sale_as_the_form_shows_it(pool: MySqlPool) {
    let res = server(pool)
        .get("/v1/sales/7010")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({
            "id": 7010,
            "customer_id": 1000,
            "seller_id": 100,
            "project_address": "1 Main St",
            "notes": "Deliver after 5pm",
            "price": "4200.00",
            "company_name": null,
            "extras": null,
            "rooms": [{
                "room_id": null,
                "room": "Kitchen",
                "backsplash": null,
                "square_feet": "30.25",
                "retail_price": null,
                "tear_out": null,
                "stove": null,
                "waterfall": null,
                "corbels": null,
                "seam": null,
                "ten_year_sealer": false,
                "extras": null,
                // It has remnants: sold partially.
                "slabs": [{ "id": 6071, "is_full": false }],
                "sinks": [
                    { "id": 6109, "type_id": 3102, "price": null },
                    { "id": 6110, "type_id": 3102, "price": null },
                    { "id": 6111, "type_id": 3107, "price": null }
                ],
                "faucets": []
            }]
        })
    );
}

#[test_db::test(seed)]
async fn a_saved_sale_reads_back_room_by_room(pool: MySqlPool) {
    let server = server(pool);
    let mut kitchen = room(
        ROOM_A,
        json!([{ "id": 6070, "is_full": true }, { "id": 6074, "is_full": false }]),
        12.5,
    );
    kitchen["sinks"] = json!([{ "type_id": 3100, "price": 199 }]);
    let mut island = room(ROOM_B, json!([{ "id": 6072, "is_full": true }]), 4.0);
    island["room"] = json!("island");
    island["ten_year_sealer"] = json!(true);
    island["corbels"] = json!(2);
    let mut body = sale(1004, json!([kitchen, island]));
    body["extras"] = json!([{ "adjustment": "Delivery", "price": 75 }]);
    let id = create(&server, &body).await;

    let got: Value = server
        .get(&format!("/v1/sales/{id}"))
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(got["customer_id"], 1004);
    assert_eq!(got["company_name"], "Erin Builders LLC");
    assert_eq!(
        got["extras"],
        json!([{ "adjustment": "Delivery", "price": 75 }])
    );
    let rooms = got["rooms"].as_array().unwrap();
    assert_eq!(rooms.len(), 2);
    assert_eq!(rooms[0]["room_id"], ROOM_A);
    assert_eq!(
        rooms[0]["slabs"],
        json!([{ "id": 6070, "is_full": true }, { "id": 6074, "is_full": false }])
    );
    assert_eq!(rooms[0]["square_feet"], "12.50");
    assert_eq!(rooms[0]["retail_price"], "50.00");
    assert_eq!(rooms[0]["extras"]["edge_price"]["edge_type"], "flat");
    assert_eq!(
        rooms[0]["sinks"],
        json!([{ "id": 6100, "type_id": 3100, "price": "199.00" }])
    );
    assert_eq!(rooms[1]["room"], "island");
    assert_eq!(rooms[1]["ten_year_sealer"], true);
    assert_eq!(rooms[1]["corbels"], 2);
    assert_eq!(rooms[1]["slabs"], json!([{ "id": 6072, "is_full": true }]));
}

#[test_db::test(seed)]
async fn get_is_404_outside_the_company_or_without_slabs(pool: MySqlPool) {
    let server = server(pool);
    // 7003: company 101's customer. 7002: no slabs. 999999: no sale.
    for id in [7003, 7002, 999_999] {
        let res = server
            .get(&format!("/v1/sales/{id}"))
            .with_session(sessions::REP)
            .await;
        res.assert_status(StatusCode::NOT_FOUND);
    }
    server
        .get("/v1/sales/7010")
        .await
        .assert_status_unauthorized();
    server
        .get("/v1/sales/7010")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

// ---- PUT /v1/sales/{id}

#[test_db::test(seed)]
async fn edit_saves_the_sale_and_reinstalls_fixtures(pool: MySqlPool) {
    let server = server(pool.clone());
    let mut kitchen = room(ROOM_A, json!([{ "id": 6071, "is_full": false }]), 31.0);
    kitchen["sinks"] = json!([{ "type_id": 3100, "price": 150 }]);
    let mut body = sale(1000, json!([kitchen]));
    body["notes"] = json!("Call first");
    let res = server
        .put("/v1/sales/7010")
        .with_session(sessions::REP)
        .json(&body)
        .await;
    res.assert_status(StatusCode::NO_CONTENT);

    let (notes, seller, sqft): (Option<String>, Option<i32>, Option<String>) = sqlx::query_as(
        "SELECT notes, seller_id, CAST(square_feet AS CHAR) FROM sales WHERE id = 7010",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    // No seller_id in the body: the seller stays.
    assert_eq!(
        (notes.as_deref(), seller, sqft.as_deref()),
        (Some("Call first"), Some(100), Some("31.00"))
    );
    let sold = slab(&pool, 6071).await.unwrap();
    assert_eq!(sold.sale_id, Some(7010));
    assert_eq!(sold.room_id.as_deref(), Some(ROOM_A));
    // The old sinks are freed, the new one installed.
    for unit in [6109, 6110, 6111] {
        assert_eq!(unit_slab(&pool, "sinks", unit).await, (None, None));
    }
    assert_eq!(
        unit_slab(&pool, "sinks", 6100).await,
        (Some(6071), Some("150.00".into()))
    );
    // Partial with remnants already: no new remnant; the remnants stay.
    assert_eq!(children(&pool, 6071).await.len(), 2);
}

#[test_db::test(seed)]
async fn edit_frees_slabs_left_out_with_their_fixtures(pool: MySqlPool) {
    let server = server(pool.clone());
    let mut kitchen = room(
        ROOM_A,
        json!([{ "id": 6070, "is_full": true }, { "id": 6074, "is_full": true }]),
        9.0,
    );
    kitchen["faucets"] = json!([{ "type_id": 3200, "price": 10 }]);
    let id = create(&server, &sale(1000, json!([kitchen]))).await;
    assert_eq!(unit_slab(&pool, "faucets", 6200).await.0, Some(6070));

    // Keep only 6074, without a faucet.
    let res = server
        .put(&format!("/v1/sales/{id}"))
        .with_session(sessions::REP)
        .json(&sale(
            1000,
            json!([room(ROOM_A, json!([{ "id": 6074, "is_full": true }]), 9.0)]),
        ))
        .await;
    res.assert_status(StatusCode::NO_CONTENT);
    let freed = slab(&pool, 6070).await.unwrap();
    assert_eq!(
        (freed.sale_id, freed.room, freed.room_id),
        (None, None, None)
    );
    assert_eq!(unit_slab(&pool, "faucets", 6200).await, (None, None));
    assert_eq!(
        slab(&pool, 6074).await.unwrap().sale_id,
        Some(i32::try_from(id).unwrap())
    );
}

#[test_db::test(seed)]
async fn edit_to_whole_slab_keeps_remnants_other_sales_bought(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .put("/v1/sales/7010")
        .with_session(sessions::REP)
        .json(&sale(
            1000,
            json!([room(ROOM_A, json!([{ "id": 6071, "is_full": true }]), 30.0)]),
        ))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // The free remnant goes, the one sale 7011 bought stays.
    assert!(slab(&pool, 6072).await.is_none());
    assert_eq!(slab(&pool, 6073).await.unwrap().sale_id, Some(7011));
}

#[test_db::test(seed)]
async fn edit_checks_the_sale_and_the_body(pool: MySqlPool) {
    let server = server(pool.clone());
    let body = sale(
        1000,
        json!([room(ROOM_A, json!([{ "id": 6071, "is_full": false }]), 5.0)]),
    );
    for id in [7003, 999_999] {
        server
            .put(&format!("/v1/sales/{id}"))
            .with_session(sessions::REP)
            .json(&body)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    // 6073 belongs to sale 7011.
    expect_400(
        &server,
        "PUT",
        "/v1/sales/7010",
        &sale(
            1000,
            json!([room(ROOM_A, json!([{ "id": 6073, "is_full": true }]), 5.0)]),
        ),
        "slab 6073 is already sold (sale 7011)",
    )
    .await;
    assert_eq!(slab(&pool, 6071).await.unwrap().sale_id, Some(7010));
    server
        .put("/v1/sales/7010")
        .json(&body)
        .await
        .assert_status_unauthorized();
}

// ---- POST /v1/sales/{id}/unsell

#[test_db::test(seed)]
async fn unsell_cancels_and_frees_everything(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .post("/v1/sales/7010/unsell")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let (status, cancelled): (String, bool) =
        sqlx::query_as("SELECT status, cancelled_date IS NOT NULL FROM sales WHERE id = 7010")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), cancelled), ("cancelled", true));
    let freed = slab(&pool, 6071).await.unwrap();
    assert_eq!(
        (freed.sale_id, freed.room, freed.square_feet),
        (None, None, None)
    );
    for unit in [6109, 6110, 6111] {
        assert_eq!(unit_slab(&pool, "sinks", unit).await, (None, None));
    }
    // Free remnant removed; the one sale 7011 bought is kept (Remix deleted it).
    assert!(slab(&pool, 6072).await.is_none());
    assert_eq!(slab(&pool, 6073).await.unwrap().sale_id, Some(7011));
}

#[test_db::test(seed)]
async fn unsell_removes_every_piece_the_sale_cut(pool: MySqlPool) {
    let server = server(pool.clone());
    let id = create(
        &server,
        &sale(
            1000,
            json!([
                room(ROOM_A, json!([{ "id": 6072, "is_full": false }]), 8.0),
                room(ROOM_B, json!([{ "id": 6072, "is_full": false }]), 6.0)
            ]),
        ),
    )
    .await;
    assert_eq!(children(&pool, 6072).await.len(), 2);
    server
        .post(&format!("/v1/sales/{id}/unsell"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(children(&pool, 6072).await.is_empty());
    assert_eq!(slab(&pool, 6072).await.unwrap().sale_id, None);
    let leftovers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM slab_inventory WHERE id > 6077")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(leftovers, 0);
}

#[test_db::test(seed)]
async fn unsell_is_scoped_and_authenticated(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .post("/v1/sales/7003/unsell")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let status: String = sqlx::query_scalar("SELECT status FROM sales WHERE id = 7003")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "sold");
    server
        .post("/v1/sales/7010/unsell")
        .await
        .assert_status_unauthorized();
    server
        .post("/v1/sales/7010/unsell")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // Company 101 can cancel its own sale.
    server
        .post("/v1/sales/7003/unsell")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
}
