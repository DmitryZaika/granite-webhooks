//! The announcement a user hears before an incoming call connects
//! ("Incoming call information" → Play announcement): the `<Sip url>` of
//! their phones' legs when they turned it on for this kind of line.

use super::dial::inbound_call;
use super::{Hook, TexmlRequest, failed, hang_up, other_party, texml};
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::IntoParams;
use crate::phone::{routing, settings};
use crate::state::AppState;
use crate::telnyx::{Response as Texml, Verb};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

/// Query of `whisper`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct WhisperParams {
    /// Our call id.
    pub call: i64,
    /// The user whose phone answered.
    pub user: i32,
    /// `1` when the user pressed a key during the announcement: connect.
    pub done: Option<String>,
}

/// Announce a call before it connects
///
/// The `<Sip url>` of a phone of a user who turned on the announcement
/// (`PUT /v1/phone/settings/call-information`): once they answer they hear
/// "Call for <line name> from <caller>" (the line's name only when they
/// chose it; the caller is the CRM customer's name, else the number read
/// out), then the call connects; a key press connects at once. A call that
/// ended, or that someone else answered meanwhile, hangs this phone up.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/whisper",
    operation_id = "announce_telnyx_incoming_call",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), WhisperParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's `<Sip url>` request for the phone's leg: `CallSid`"),
    responses(
        (status = 200, description = "TeXML: the announcement (then the call connects), or hang up", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request, or no `call` / `user`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn whisper(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<WhisperParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "whisper").await?;
    if params.done.as_deref() == Some("1") {
        return Ok(Texml::new().into_response());
    }
    Ok(match announce(&state, &hook, &params).await {
        Ok(response) => response,
        Err(err) => {
            // Better the call without its announcement than no call.
            failed("whisper", hook.company_id(), &err);
            Texml::new().into_response()
        }
    })
}

async fn announce(state: &AppState, hook: &Hook, params: &WhisperParams) -> ApiResult<Response> {
    let pool = &state.pool;
    let call = inbound_call(state, hook, params.call).await?;
    hook.claim(state, "whisper", Some(call.id)).await?;
    let someone_else = call.answered && call.user_id.is_some_and(|user| user != params.user);
    if call.ended || someone_else {
        return Ok(hang_up());
    }
    let stored = settings::load(pool, params.user, hook.company_id()).await?;
    let line = match call.number_id {
        Some(number_id) => routing::line(pool, hook.company_id(), number_id).await?,
        None => None,
    };
    let line_name = line
        .map(|line| line.label_or_number())
        .filter(|_| stored.announce_line_name);
    let who = other_party(state, &call).await?;
    let text = line_name.map_or_else(
        || format!("Call from {who}."),
        |name| format!("Call for {name} from {who}."),
    );
    let gather = Verb::new("Gather")
        .attr("numDigits", "1")
        .attr("timeout", "1")
        .attr(
            "action",
            format!(
                "{}/whisper?call={}&user={}&done=1",
                hook.base, call.id, params.user
            ),
        )
        .child(Verb::new("Say").text(text));
    Ok(Texml::new().verb(gather).into_response())
}
