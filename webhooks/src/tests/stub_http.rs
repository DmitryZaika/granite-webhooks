//! Loopback HTTP stubs so tests never reach the real `CloudTalk` or Google APIs.
//!
//! Production code asks [`cloudtalk_base_url`] / [`google_places_base_url`] for its
//! endpoint; a test points them at a local server with [`StubServer::route`] and
//! [`use_cloudtalk`] / [`use_google_places`]. The overrides are thread-local, and
//! `#[sqlx::test]` / `#[tokio::test]` run each test on its own current-thread
//! runtime, so tests do not see each other's stubs.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::response::IntoResponse;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

thread_local! {
    static CLOUDTALK_BASE: RefCell<Option<String>> = const { RefCell::new(None) };
    static GOOGLE_PLACES_BASE: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub fn cloudtalk_base_url() -> Option<String> {
    CLOUDTALK_BASE.with(|cell| cell.borrow().clone())
}

pub fn google_places_base_url() -> Option<String> {
    GOOGLE_PLACES_BASE.with(|cell| cell.borrow().clone())
}

pub fn use_cloudtalk(base: &str) {
    CLOUDTALK_BASE.with(|cell| *cell.borrow_mut() = Some(base.to_string()));
}

pub fn use_google_places(base: &str) {
    GOOGLE_PLACES_BASE.with(|cell| *cell.borrow_mut() = Some(base.to_string()));
}

/// One request the stub received.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    /// Path plus query string.
    pub path: String,
    pub body: String,
}

type Responder = Arc<dyn Fn(&Recorded) -> Option<serde_json::Value> + Send + Sync>;

#[derive(Clone)]
struct StubState {
    seen: Arc<Mutex<Vec<Recorded>>>,
    responder: Responder,
}

pub struct StubServer {
    /// `http://127.0.0.1:<port>`
    pub base: String,
    seen: Arc<Mutex<Vec<Recorded>>>,
}

impl StubServer {
    /// Starts a server on an ephemeral loopback port. `responder` returns the JSON
    /// body for a request, or `None` for a 404.
    pub async fn start<F>(responder: F) -> Self
    where
        F: Fn(&Recorded) -> Option<serde_json::Value> + Send + Sync + 'static,
    {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = StubState {
            seen: seen.clone(),
            responder: Arc::new(responder),
        };
        let app = Router::new().fallback(handle).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub server");
        let base = format!("http://{}", listener.local_addr().expect("stub address"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve stub");
        });
        Self { base, seen }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.seen.lock().expect("stub lock").clone()
    }
}

async fn handle(State(state): State<StubState>, request: Request) -> impl IntoResponse {
    let method = request.method().to_string();
    let path = request
        .uri()
        .path_and_query()
        .map(ToString::to_string)
        .unwrap_or_default();
    let body = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|_| Bytes::new());
    let recorded = Recorded {
        method,
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    let reply = (state.responder)(&recorded);
    state.seen.lock().expect("stub lock").push(recorded);
    match reply {
        Some(json) => axum::Json(json).into_response(),
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

fn component(long: &str, short: &str, kind: &str) -> serde_json::Value {
    serde_json::json!({ "longText": long, "shortText": short, "types": [kind] })
}

/// Google Places stub that resolves any query to one fixed Carmel, IN address.
pub fn carmel_places_responder(recorded: &Recorded) -> Option<serde_json::Value> {
    if recorded.path.ends_with("/places:autocomplete") {
        return Some(serde_json::json!({
            "suggestions": [{
                "placePrediction": {
                    "text": { "text": "2001 East Greyhound Pass, Carmel, IN, USA" },
                    "placeId": "stub-place"
                }
            }]
        }));
    }
    if recorded.path.ends_with("/places/stub-place") {
        let part = component;
        return Some(serde_json::json!({
            "addressComponents": [
                part("2001", "2001", "street_number"),
                part("East Greyhound Pass", "E Greyhound Pass", "route"),
                part("Carmel", "Carmel", "locality"),
                part("Indiana", "IN", "administrative_area_level_1"),
                part("46033", "46033", "postal_code"),
            ]
        }));
    }
    None
}
