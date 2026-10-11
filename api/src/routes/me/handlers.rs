use super::queries::{
    PositionRow, clear_telegram_id, positions_in_company, sales_goal as stored_sales_goal,
    save_sales_goal, telegram_ids,
};
use super::schemas::{SalesGoal, TelegramStatus, UpdateSalesGoal};
use crate::auth::session::SessionUser;
use crate::auth::{CurrentUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use crate::telegram::Bot;
use axum::Json;
use axum::extract::State;

/// `users.monthly_sales_goal` is DECIMAL(12, 2).
const MAX_SALES_GOAL: f64 = 10_000_000_000.0;

/// Get the signed-in user
///
/// Returns the user behind the session, including their permission flags and
/// the company every other request acts in.
#[utoipa::path(
    get,
    path = "/v1/me",
    operation_id = "get_current_user",
    tag = "me",
    responses(
        (status = 200, description = "The signed-in user", body = SessionUser),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn me(CurrentUser(user): CurrentUser) -> Json<SessionUser> {
    Json(user)
}

/// List the signed-in user's positions
///
/// Returns the position ids (job roles such as sales rep) the user holds in
/// the company the request acts in.
#[utoipa::path(
    get,
    path = "/v1/me/positions",
    operation_id = "list_current_user_positions",
    tag = "me",
    responses(
        (status = 200, description = "Position ids in the current company", body = Vec<PositionRow>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn positions(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Vec<PositionRow>>> {
    let rows = positions_in_company(&state.pool, user.id, user.company_id).await?;
    Ok(Json(rows))
}

/// Get my monthly sales goal
///
/// Returns the amount the signed-in user aims to sell each month, or null
/// when they have not set one. Set it with `PUT /v1/me/sales-goal`.
#[utoipa::path(
    get,
    path = "/v1/me/sales-goal",
    operation_id = "get_my_sales_goal",
    tag = "me",
    responses(
        (status = 200, description = "The goal, null when not set", body = SalesGoal),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn sales_goal(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<SalesGoal>> {
    Ok(Json(stored_sales_goal(&state.pool, user.id).await?))
}

/// Set my monthly sales goal
///
/// Saves the amount the signed-in user aims to sell each month (USD, above 0,
/// rounded to cents) and returns it as saved.
#[utoipa::path(
    put,
    path = "/v1/me/sales-goal",
    operation_id = "set_my_sales_goal",
    tag = "me",
    request_body = UpdateSalesGoal,
    responses(
        (status = 200, description = "The goal as saved", body = SalesGoal),
        (status = 400, description = "The amount is not above 0 or too large", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_sales_goal(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(body): JsonBody<UpdateSalesGoal>,
) -> ApiResult<Json<SalesGoal>> {
    let amount = body.monthly_sales_goal;
    // Rounds to 0.00 below half a cent, which would read back as "not set".
    if !amount.is_finite() || amount < 0.005 || amount >= MAX_SALES_GOAL {
        return Err(ApiError::BadRequest(
            "monthly_sales_goal must be above 0 and below 10000000000".into(),
        ));
    }
    save_sales_goal(&state.pool, user.id, amount).await?;
    Ok(Json(stored_sales_goal(&state.pool, user.id).await?))
}

/// Get my Telegram link status
///
/// Returns whether the signed-in user's Telegram is linked to the lead alerts
/// bot and to the notifications bot, and the bots' ids for the Telegram login
/// widget. When a bot is configured, the saved link is checked with Telegram
/// first; a link Telegram rejects (bot blocked, chat gone) is removed and
/// reported as not linked, so the user is asked to connect again.
#[utoipa::path(
    get,
    path = "/v1/me/telegram",
    operation_id = "get_my_telegram_status",
    tag = "me",
    responses(
        (status = 200, description = "Link status per bot", body = TelegramStatus),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn telegram(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<TelegramStatus>> {
    let ids = telegram_ids(&state.pool, user.id).await?;
    let bots = &state.telegram;
    let check = async |bot, chat_id: &Option<String>| match chat_id {
        Some(chat_id) => bots.can_message(bot, chat_id).await,
        None => None,
    };
    // The two bots are independent: ask Telegram about both at once.
    let (leads_ok, notifications_ok) = tokio::join!(
        check(Bot::Leads, &ids.telegram_id),
        check(Bot::Notifications, &ids.notifications_telegram_id),
    );
    let mut status = TelegramStatus {
        leads_linked: ids.telegram_id.is_some(),
        notifications_linked: ids.notifications_telegram_id.is_some(),
        leads_bot_id: bots.bot_id(Bot::Leads),
        notifications_bot_id: bots.bot_id(Bot::Notifications),
    };
    if leads_ok == Some(false) {
        clear_telegram_id(&state.pool, user.id, Bot::Leads).await?;
        status.leads_linked = false;
    }
    if notifications_ok == Some(false) {
        clear_telegram_id(&state.pool, user.id, Bot::Notifications).await?;
        status.notifications_linked = false;
    }
    Ok(Json(status))
}
