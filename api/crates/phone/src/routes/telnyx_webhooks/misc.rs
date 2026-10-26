//! The calls our server places for the app's own checks: the test call to a
//! phone (`POST /v1/phone/calls/test`) and the cell verification code
//! (`POST /v1/phone/cell/verify`).

use super::{Hook, TexmlRequest, failed, hang_up, texml};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::IntoParams;
use crate::phone::call_log::{self, Answer};
use crate::phone::numbers::last10;
use crate::state::AppState;
use crate::telnyx::{Response as Texml, Verb};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

/// What the test call says.
const TEST_MESSAGE: &str = "This is a Granite Phone test call. Your phone can take calls. Goodbye.";
/// What the code call says when there is no code to read.
const NO_CODE: &str =
    "This code is no longer valid. Ask for a new one in the Granite Phone app. Goodbye.";

/// Query of `test-call`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TestCallParams {
    /// Our call id (the test call).
    pub call: i64,
}

/// Query of `cell-code`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CellCodeParams {
    /// The user whose cell is being verified.
    pub user: i32,
}

/// Speak a test call
///
/// The TeXML our test call fetches when the phone answers: a short message,
/// then hang up. The test call is logged as answered (it never shows in
/// Recents or stats). Any other call hangs up.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/test-call",
    operation_id = "speak_telnyx_test_call",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), TestCallParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's request when the phone answered: `CallSid`, `CallStatus`"),
    responses(
        (status = 200, description = "TeXML: the test message, then hang up", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request, or no `call`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn test_call(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<TestCallParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "test-call").await?;
    Ok(match greet(&state, &hook, params.call).await {
        Ok(response) => response,
        Err(err) => {
            failed("test-call", hook.company_id(), &err);
            hang_up()
        }
    })
}

async fn greet(state: &AppState, hook: &Hook, call_id: i64) -> ApiResult<Response> {
    let call = call_log::facts(&state.pool, hook.company_id(), call_id)
        .await?
        .filter(call_log::CallFacts::is_test)
        .ok_or(ApiError::NotFound)?;
    let claim = hook.claim(state, "test-call", Some(call.id)).await?;
    if claim.new {
        call_log::answered(&state.pool, call.company_id, call.id, Answer::default()).await?;
    }
    Ok(Texml::new().say(TEST_MESSAGE).hangup().into_response())
}

/// Read a cell verification code
///
/// The TeXML our code call to the user's new cell fetches when it answers:
/// the user's pending 6-digit code, read twice, then hang up. Nothing is
/// read when the code expired or was used, when the user is not the
/// token's company's, or when the request's `To` is missing or not the cell
/// being verified.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/cell-code",
    operation_id = "read_telnyx_cell_code",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), CellCodeParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's request when the cell answered: `CallSid`, `To`"),
    responses(
        (status = 200, description = "TeXML: the code, or that there is none", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request, or no `user`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn cell_code(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<CellCodeParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "cell-code").await?;
    Ok(match read_code(&state, &hook, params.user).await {
        Ok(response) => response,
        Err(err) => {
            failed("cell-code", hook.company_id(), &err);
            hang_up()
        }
    })
}

async fn read_code(state: &AppState, hook: &Hook, user_id: i32) -> ApiResult<Response> {
    hook.claim(state, "cell-code", None).await?;
    let pending: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT s.cell_code, s.cell_pending_number
           FROM phone_user_settings s
           JOIN users u ON u.id = s.user_id AND u.company_id = s.company_id AND u.is_deleted = 0
          WHERE s.user_id = ? AND s.company_id = ? AND s.cell_code IS NOT NULL
            AND s.cell_code_expires_at > UTC_TIMESTAMP()",
    )
    .bind(user_id)
    .bind(hook.company_id())
    .fetch_optional(&state.pool)
    .await?;
    // Only to the cell being verified: a request that names no number, or
    // another one, hears nothing.
    let called = hook.form.to().and_then(last10);
    let code = pending.and_then(|(code, cell)| {
        let to_that_cell = called.is_some() && called == cell.as_deref().and_then(last10);
        (to_that_cell && code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()))
            .then_some(code)
    });
    let Some(code) = code else {
        return Ok(Texml::new().say(NO_CODE).hangup().into_response());
    };
    let spoken = code
        .chars()
        .map(String::from)
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Texml::new()
        .say(&format!("Your Granite Phone code is {spoken}."))
        .verb(Verb::new("Pause").attr("length", "1"))
        .say(&format!("Again, your code is {spoken}."))
        .hangup()
        .into_response())
}
