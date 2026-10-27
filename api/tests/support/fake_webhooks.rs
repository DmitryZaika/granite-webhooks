//! A stand-in for the `webhooks` Lambda: the emails and Telegram messages the
//! api sends. Include it with `#[path = "../support/fake_webhooks.rs"] mod fake_webhooks;` in the area's `main.rs`.
#![allow(dead_code)]

use api::webhooks_client::WebhooksClient;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Sink {
    /// Bodies of `POST /email/send`.
    pub emails: Vec<Value>,
    /// Bodies of `POST /telegram/notifications-notify`.
    pub telegrams: Vec<Value>,
    /// Answer the next emails with 502.
    pub fail_emails: u32,
}

pub type Shared = Arc<Mutex<Sink>>;

impl Sink {
    /// Emails sent to `to`.
    pub fn emails_to(&self, to: &str) -> Vec<Value> {
        self.emails
            .iter()
            .filter(|email| {
                email["to"]
                    .as_array()
                    .is_some_and(|list| list.iter().any(|a| a == to))
            })
            .cloned()
            .collect()
    }
}

async fn email(State(sink): State<Shared>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let mut sink = sink.lock().unwrap();
    if sink.fail_emails > 0 {
        sink.fail_emails -= 1;
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "fake outage" })),
        );
    }
    sink.emails.push(body);
    let id = format!("<fake-{}@test>", sink.emails.len());
    (StatusCode::OK, Json(json!({ "message_id": id })))
}

async fn telegram(State(sink): State<Shared>, Json(body): Json<Value>) -> StatusCode {
    sink.lock().unwrap().telegrams.push(body);
    StatusCode::OK
}

/// Serves the sink on a free port; answers a client for it.
pub async fn start(sink: &Shared) -> WebhooksClient {
    let router = Router::new()
        .route("/email/send", post(email))
        .route("/telegram/notifications-notify", post(telegram))
        .with_state(Arc::clone(sink));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    WebhooksClient::new(Some(&format!("http://{address}")), Some("fake-key"))
}
