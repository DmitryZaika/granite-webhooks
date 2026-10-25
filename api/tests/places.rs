use crate::common;

use api::app;
use api::maps::Maps;
use api::state::AppState;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn server(pool: MySqlPool) -> TestServer {
    let router = Router::new()
        .route(
            "/places:autocomplete",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["includedRegionCodes"], json!(["us"]));
                Json(json!({ "suggestions": [
                    { "placePrediction": { "placeId": "p1", "text": { "text": "1 Main St, Carmel, IN 46032, USA" } } },
                    { "placePrediction": { "placeId": "p2", "text": { "text": "9 Elm St, Fishers, IN, USA" } } },
                ] }))
            }),
        )
        .route(
            "/places/{id}",
            get(|axum::extract::Path(id): axum::extract::Path<String>| async move {
                Json(json!({
                    "id": id, "displayName": { "text": "9 Elm St" },
                    "formattedAddress": "9 Elm St, Fishers, IN 46038, USA",
                    "addressComponents": [
                        { "longText": "Fishers", "shortText": "Fishers", "types": ["locality"] },
                        { "longText": "Indiana", "shortText": "IN", "types": ["administrative_area_level_1"] },
                        { "longText": "United States", "shortText": "US", "types": ["country"] },
                    ],
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{address}");
    let maps = Maps::new(Some("key"), &format!("{base}/matrix"), &base).with_places_base(&base);
    TestServer::new(app(
        AppState::new(pool, SECRET, ALLOWED_ORIGIN).with_maps(maps)
    ))
}

#[test_db::test(seed)]
async fn suggestions_come_with_their_zip(pool: MySqlPool) {
    let server = server(pool).await;
    let found: Value = server
        .get("/v1/places/autocomplete?q=main")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        found[0]["address"],
        json!({ "street": "1 Main St", "city": "Carmel", "state": "IN", "zip": "46032" })
    );
    // No ZIP in the text: it comes from the place.
    assert_eq!(found[1]["place_id"], "p2");
    assert_eq!(found[1]["address"]["zip"], "46038");
    assert_eq!(
        server
            .get("/v1/places/autocomplete?q=ma")
            .with_session(sessions::REP)
            .await
            .json::<Value>(),
        json!([])
    );
    server
        .get("/v1/places/autocomplete?q=main")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    let place: Value = server
        .get("/v1/places/p2")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        place,
        json!({
            "place_id": "p2", "name": "9 Elm St", "formatted_address": "9 Elm St, Fishers, IN 46038, USA",
            "zip_code": "46038", "city": "Fishers", "state": "IN", "country": "US",
        })
    );
}
