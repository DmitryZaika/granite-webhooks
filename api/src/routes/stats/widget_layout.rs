//! The admin Statistics page's widget layout.
//!
//! - `GET /v1/stats/widget-layout` → the saved layout and whether new boards start empty
//! - `PUT /v1/stats/widget-layout` → save the layout
//!
//! The layout lives in the browser's `admin-statistics-widgets-<company>`
//! cookie (`HttpOnly`, a year), as the Remix page kept it, so nobody loses
//! their board. The page parses and validates the value itself
//! (`statisticsWidgets.ts`).

use crate::auth::AdminUser;
use crate::auth::cookie::{cookie_value, decode_plain, encode_plain};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

const ONE_YEAR: i64 = 60 * 60 * 24 * 365;
/// A layout is a few dozen widget ids; anything bigger is not one.
const MAX_BYTES: usize = 3000;

fn cookie_name(company_id: i32) -> String {
    format!("admin-statistics-widgets-{company_id}")
}

/// `GET /v1/stats/widget-layout`.
#[derive(Debug, Serialize, ToSchema)]
pub struct WidgetLayout {
    /// The layout this browser saved for the company, as the page stored it;
    /// null when none.
    #[schema(value_type = Option<Object>)]
    pub saved: Option<Value>,
    /// The company starts with an empty board (new companies) instead of the
    /// default widgets.
    pub hidden_by_default: bool,
}

/// Get the Statistics widget layout
///
/// The widget layout this browser saved for the company (its cookie), and
/// whether a board without one starts empty.
#[utoipa::path(
    get,
    path = "/v1/stats/widget-layout",
    operation_id = "get_statistics_widget_layout",
    tag = "stats",
    responses(
        (status = 200, description = "The layout", body = WidgetLayout),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_layout(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    headers: HeaderMap,
) -> ApiResult<Json<WidgetLayout>> {
    let saved = headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .find_map(|header| cookie_value(header, &cookie_name(user.company_id)))
        .and_then(|value| decode_plain(&value));
    let hidden: Option<bool> = sqlx::query_scalar(
        "SELECT COALESCE(statistics_widgets_hidden, 0) = 1 FROM company WHERE id = ?",
    )
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(Json(WidgetLayout {
        saved,
        hidden_by_default: hidden.unwrap_or(false),
    }))
}

/// Body of `PUT /v1/stats/widget-layout`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SaveWidgetLayout {
    /// The whole layout as the page serializes it.
    #[schema(value_type = Object)]
    pub value: Value,
}

/// Save the Statistics widget layout
///
/// Stores the whole layout in this browser's cookie for the company.
#[utoipa::path(
    put,
    path = "/v1/stats/widget-layout",
    operation_id = "save_statistics_widget_layout",
    tag = "stats",
    request_body = SaveWidgetLayout,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Too big to be a layout", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_layout(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<SaveWidgetLayout>,
) -> ApiResult<Response> {
    let encoded = encode_plain(&input.value);
    if encoded.len() > MAX_BYTES {
        return Err(ApiError::BadRequest("layout is too big".into()));
    }
    let cookie = state
        .cookie
        .set_cookie(&cookie_name(user.company_id), &encoded, ONE_YEAR);
    Ok((StatusCode::NO_CONTENT, [(SET_COOKIE, cookie)]).into_response())
}
