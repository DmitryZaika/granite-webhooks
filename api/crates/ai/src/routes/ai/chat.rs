//! `POST /v1/ai/chat`: the AI chat's answer, streamed as server-sent events.
//!
//! Events, as the Remix `api.chat` route sent them: `ping` (thirty first,
//! so proxies flush), `info`, `status` (`{"state":"searching"|"answering"}`),
//! unnamed `message` pieces of the answer ending with `[DONE]`, `replace`
//! (drop the answer so far), and JSON events `instructions`, `sources`,
//! `images`, `actions`, `trace`, `thicknessOptions`, `slabSizeOptions`,
//! `priceListOptions`, `specialOrderOffer`, `clarify` (the choices of a
//! question back: the app or the company's own process).
//!
//! An answer can take longer than API Gateway allows: the browser calls this
//! on the streaming Function URL (`LAMBDA_RESPONSE_STREAMING=1`).

use crate::auth::CurrentUser;
use crate::chat::markers::{SpecialOrderOffer, price_as_per_sqft};
use crate::chat::{self, Event};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::response::sse::{self, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::convert::Infallible;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

/// A special order the user is pricing (from the previous answer).
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpecialOrderInput {
    /// Price per sq ft.
    pub price_per_sqft: f64,
    /// Slab length in inches, when known.
    #[serde(default)]
    pub length_inches: Option<f64>,
    /// Slab width in inches, when known.
    #[serde(default)]
    pub width_inches: Option<f64>,
}

/// Body of `POST /v1/ai/chat`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatInput {
    /// The question.
    pub query: String,
    /// `instructions` (default), `priceLists` or `assistant`. A question
    /// about the company's records goes to the assistant anyway. The
    /// assistant reads customers, deals and notes: employees only (403 for
    /// others, whose record questions are answered from the instructions).
    #[serde(default)]
    pub mode: Option<String>,
    /// The first question of a conversation.
    #[serde(default)]
    pub is_new: bool,
    /// The special order the slabs and delivery in the question are for.
    #[serde(default)]
    pub special_order: Option<SpecialOrderInput>,
}

/// `parseSpecialOrderParams`: a positive price, and a full slab size or none
/// (a slab's total price becomes a price per sq ft).
fn special_order(input: SpecialOrderInput) -> Option<SpecialOrderOffer> {
    let price = input.price_per_sqft;
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    match (input.length_inches, input.width_inches) {
        (None, None) => Some(SpecialOrderOffer {
            price_per_sqft: price,
            length_inches: None,
            width_inches: None,
        }),
        (Some(l), Some(w)) if l.is_finite() && w.is_finite() && l > 0.0 && w > 0.0 => {
            Some(SpecialOrderOffer {
                price_per_sqft: price_as_per_sqft(price, Some(l), Some(w)),
                length_inches: Some(l),
                width_inches: Some(w),
            })
        }
        _ => None,
    }
}

fn sse_event(event: Event) -> sse::Event {
    match event {
        Event::Ping => sse::Event::default().event("ping").data(""),
        Event::Info(text) => sse::Event::default().event("info").data(text),
        Event::Status(state) => sse::Event::default()
            .event("status")
            .data(serde_json::json!({ "state": state }).to_string()),
        Event::Text(text) => sse::Event::default().data(text),
        Event::Replace => sse::Event::default().event("replace").data("1"),
        Event::Json(name, json) => sse::Event::default().event(name).data(json),
        Event::Done => sse::Event::default().data(chat::DONE),
    }
}

/// Ask the AI chat
///
/// Answers a question in instructions mode (company instructions and the
/// CRM manual), price lists mode (supplier price files, special orders) or
/// assistant mode (the company's records, employees only), streamed as
/// server-sent events. The conversation is kept per user; `isNew` starts it
/// over.
#[crate::openapi::path(
    post,
    path = "/v1/ai/chat",
    operation_id = "ask_chat",
    tag = "ai",
    request_body = ChatInput,
    responses(
        (status = 200, description = "The answer as server-sent events", content_type = "text/event-stream", body = String),
        (status = 400, description = "No question", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Assistant mode asked for by a user who is not an employee", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn ask(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    JsonBody(input): JsonBody<ChatInput>,
) -> ApiResult<Response> {
    if input.query.trim().is_empty() && input.special_order.is_none() {
        return Err(ApiError::BadRequest("Ask a question".into()));
    }
    if input.mode.as_deref() == Some("assistant") && !chat::may_use_assistant(&user) {
        return Err(ApiError::Forbidden);
    }
    let request = chat::Request {
        query: input.query,
        mode: input.mode,
        is_new: input.is_new,
        special_order: input.special_order.and_then(special_order),
    };
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(chat::answer(state, user, request, tx));
    let stream = ReceiverStream::new(rx).map(|event| Ok::<_, Infallible>(sse_event(event)));
    let mut response = Sse::new(stream).into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}
