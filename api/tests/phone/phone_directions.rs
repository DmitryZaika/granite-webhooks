//! `GET /v1/phone/directions`: where an address is, so the app's directions
//! start driving at once.
//!
//! Fixture phones use n = 341..=342.

use crate::phone_common;

use api::maps::Maps;
use axum::routing::get;
use axum::{Json, Router};
use axum_test::TestServer;
use phone_common::{HEADER, add_session, phone_server_with, token};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;

async fn server(pool: &MySqlPool) -> TestServer {
    let google = Router::new().route(
        "/geocode/json",
        get(
            |axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>| async move {
                Json(match q["address"].as_str() {
                    "6309 Looking Glass Lane, Indianapolis, IN, 46235" => json!({
                        "status": "OK",
                        "results": [{ "geometry": { "location": { "lat": 39.86, "lng": -85.97 } } }],
                    }),
                    _ => json!({ "status": "ZERO_RESULTS", "results": [] }),
                })
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, google).await.unwrap() });
    let base = format!("http://{address}");
    phone_server_with(pool.clone(), move |state| {
        state.with_maps(Maps::new(Some("key"), &format!("{base}/matrix"), &base))
    })
    .await
    .0
}

#[test_db::test(seed)]
async fn the_phone_gets_where_an_address_is(pool: MySqlPool) {
    add_session(&pool, 341, 100, 100).await;
    let server = server(&pool).await;

    let found = server
        .get("/v1/phone/directions")
        .add_query_param("address", " 6309 Looking Glass Lane, Indianapolis, IN, 46235 ")
        .add_header(HEADER, token(341))
        .await;
    found.assert_status_ok();
    assert_eq!(found.json::<Value>(), json!({ "lat": 39.86, "lng": -85.97 }));

    // Google can't find it: 404, and the app passes the words instead.
    server
        .get("/v1/phone/directions")
        .add_query_param("address", "Nowhere at all")
        .add_header(HEADER, token(341))
        .await
        .assert_status_not_found();
    // Nothing, or too much, to look up.
    for address in ["  ".to_string(), "a".repeat(301)] {
        server
            .get("/v1/phone/directions")
            .add_query_param("address", &address)
            .add_header(HEADER, token(341))
            .await
            .assert_status_bad_request();
    }
}

#[test_db::test(seed)]
async fn only_a_signed_in_phone_asks(pool: MySqlPool) {
    let server = server(&pool).await;
    server
        .get("/v1/phone/directions")
        .add_query_param("address", "6309 Looking Glass Lane, Indianapolis, IN, 46235")
        .add_header(HEADER, token(342))
        .await
        .assert_status_unauthorized();
    server
        .get("/v1/phone/directions")
        .add_query_param("address", "6309 Looking Glass Lane, Indianapolis, IN, 46235")
        .await
        .assert_status_unauthorized();
}
