use crate::common;

use api::app;
use api::maps::Maps;
use api::state::AppState;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test_db::test(seed)]
async fn miles_come_from_the_route_matrix(pool: MySqlPool) {
    sqlx::query("UPDATE company SET address = '1 Main St, Indianapolis' WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let router = Router::new().route(
        "/matrix",
        post(move |Json(body): Json<Value>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    body["origins"][0]["waypoint"]["address"],
                    "1 Main St, Indianapolis"
                );
                let to = body["destinations"][0]["waypoint"]["address"]
                    .as_str()
                    .unwrap();
                Json(if to == "Nowhere" {
                    json!([{ "condition": "ROUTE_NOT_FOUND" }])
                } else {
                    json!([{ "condition": "ROUTE_EXISTS", "distanceMeters": 39_107 }])
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN).with_maps(Maps::new(
        Some("key"),
        &format!("http://{address}/matrix"),
        &format!("http://{address}"),
    ));
    let server = TestServer::new(app(state));
    let miles = |to: &str| {
        let request = server
            .get("/v1/company/drive-miles")
            .add_query_param("address", to)
            .with_session(sessions::REP);
        async move { request.await.json::<Value>()["miles"].clone() }
    };
    assert_eq!(miles("Fishers").await, json!(24.3));
    assert_eq!(miles("fishers ").await, json!(24.3));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the second ask is cached");
    assert_eq!(miles("Nowhere").await, Value::Null);
    assert_eq!(miles(" ").await, Value::Null);
}
