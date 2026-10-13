//! AI Design: `GET /v1/ai/design/stones` and the streamed `POST
//! /v1/ai/design`, against `OpenAI` stand-ins for the slab description and
//! the image edit.

mod common;

use api::ai::Ai;
use api::app;
use api::state::AppState;
use api::storage::Storage;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, png, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct OpenAi {
    chats: Arc<Mutex<Vec<Value>>>,
    edits: Arc<Mutex<Vec<String>>>,
}

async fn chat(State(ai): State<OpenAi>, Json(request): Json<Value>) -> Json<Value> {
    let taj = request.to_string().contains("Taj Mahal");
    ai.chats.lock().unwrap().push(request);
    let description = if taj {
        "- warm taj beige"
    } else {
        "- jet black base\n- fine white veins"
    };
    Json(json!({
        "choices": [{ "message": { "content": description }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 900, "completion_tokens": 40 },
    }))
}

async fn edit(State(ai): State<OpenAi>, body: Bytes) -> Response {
    let text = String::from_utf8_lossy(&body).to_string();
    ai.edits.lock().unwrap().push(text.clone());
    if text.contains("taj beige") {
        return (
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"Your request was rejected by the safety system."}}"#,
        )
            .into_response();
    }
    Json(json!({
        "data": [{ "b64_json": "aGVsbG8=" }],
        "output_format": "png",
        "usage": { "input_tokens": 1000, "output_tokens": 500,
                   "input_tokens_details": { "image_tokens": 800 }, "output_tokens_details": { "image_tokens": 400 } },
    }))
    .into_response()
}

async fn setup(pool: &MySqlPool) -> (TestServer, OpenAi) {
    let ai = OpenAi::default();
    let router = Router::new()
        .route("/chat", post(chat))
        .route("/images", post(edit))
        .with_state(ai.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let storage = Storage::memory();
    storage
        .put("stones/slab.png", png(1200, 900), "image/png")
        .await
        .unwrap();
    let url = storage.public_url("stones/slab.png").unwrap();
    sqlx::query("UPDATE stones SET url = ? WHERE id IN (3000, 3010)")
        .bind(&url)
        .execute(pool)
        .await
        .unwrap();
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_storage(storage)
        .with_ai(
            Ai::new(Some("sk"), &format!("{base}/chat")).with_images_url(&format!("{base}/images")),
        );
    (TestServer::new(app(state)), ai)
}

fn kitchen() -> String {
    format!("data:image/png;base64,{}", STANDARD.encode(png(400, 300)))
}

#[test_db::test(seed)]
async fn each_stone_gets_its_design_or_its_error(pool: MySqlPool) {
    let (server, ai) = setup(&pool).await;
    let res = server
        .post("/v1/ai/design")
        .with_session(sessions::REP)
        .json(&json!({ "kitchen": kitchen(), "stoneIds": [3000, 3010, 3000], "surfaces": ["countertops", "fireplace"],
                       "instructions": "white cabinets" }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.header("content-type"), "application/x-ndjson");
    let events: Vec<Value> = res
        .text()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        events[0],
        json!({ "type": "status", "status": "generating" })
    );
    assert!(events.contains(&json!({
        "type": "design", "stoneId": 3000, "stoneName": "Absolute Black", "image": "data:image/png;base64,aGVsbG8=",
    })));
    let failed = events.iter().find(|e| e["type"] == "stoneError").unwrap();
    assert_eq!(failed["stoneName"], "Taj Mahal");
    assert!(failed["error"].as_str().unwrap().contains("safety system"));
    assert_eq!(
        events.last().unwrap(),
        &json!({ "type": "done", "count": 1 })
    );

    // The slab was described from its picture, then sent with the kitchen.
    let chats = ai.chats.lock().unwrap().clone();
    assert_eq!(chats.len(), 2);
    let parts = &chats[0]["messages"][0]["content"];
    assert!(
        parts[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    let edits = ai.edits.lock().unwrap().clone();
    let black = edits.iter().find(|e| e.contains("jet black")).unwrap();
    assert_eq!(black.matches("name=\"image[]\"").count(), 2);
    assert!(black.contains("Image 2 = inventory slab photo."));
    assert!(black.contains("- jet black base"));
    assert!(black.contains("- fireplace surround and hearth"));
    assert!(black.contains("USER REQUESTED CHANGES"));
    assert!(black.contains("name=\"size\"\r\n\r\n1536x1024"));
    let images: Vec<(String, bool, Option<i64>)> = sqlx::query_as(
        "SELECT feature, CAST(success AS SIGNED) = 1, CAST(image_output_tokens AS SIGNED) FROM ai_usage_log WHERE api_kind = 'image' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(images.len(), 2);
    assert!(images.contains(&("ai_design".into(), true, Some(400))));
    assert!(images.contains(&("ai_design".into(), false, None)));
}

#[test_db::test(seed)]
async fn a_design_needs_a_picture_and_stones_with_pictures(pool: MySqlPool) {
    let (server, _) = setup(&pool).await;
    let post = |body: Value| {
        server
            .post("/v1/ai/design")
            .with_session(sessions::REP)
            .json(&body)
    };
    let res = post(json!({ "kitchen": kitchen(), "stoneIds": [] })).await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "Select a stone.");
    let res = post(json!({ "kitchen": kitchen(), "stoneIds": [3000, 3001, 3002, 3003] })).await;
    assert_eq!(res.json::<Value>()["error"], "Maximum 3 stone per request.");
    let res = post(json!({ "kitchen": "data:text/plain;base64,aGk=", "stoneIds": [3000] })).await;
    assert_eq!(
        res.json::<Value>()["error"],
        "Only image files are allowed."
    );
    sqlx::query("UPDATE stones SET url = '' WHERE id = 3000")
        .execute(&pool)
        .await
        .unwrap();
    let res = post(json!({ "kitchen": kitchen(), "stoneIds": [3000] })).await;
    assert_eq!(
        res.json::<Value>()["error"],
        "The selected stones do not have a reference image."
    );
    // Another company's stone is not found either.
    let res = server
        .post("/v1/ai/design")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "kitchen": kitchen(), "stoneIds": [3010] }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn the_stones_are_the_displayed_ones_in_stock(pool: MySqlPool) {
    let (server, _) = setup(&pool).await;
    let res = server
        .get("/v1/ai/design/stones")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    let stones = res.json::<Value>()["stones"].clone();
    let names: Vec<&str> = stones
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Absolute Black"));
    // Taj Mahal is not shown in the catalog.
    assert!(!names.contains(&"Taj Mahal"));
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    let res = server
        .get("/v1/ai/design/stones")
        .add_query_param("q", "absolute")
        .with_session(sessions::REP)
        .await;
    assert_eq!(
        res.json::<Value>()["stones"],
        json!([{ "id": 3000, "name": "Absolute Black", "type": "granite" }])
    );
}
