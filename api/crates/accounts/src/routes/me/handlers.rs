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
#[crate::openapi::path(
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
#[crate::openapi::path(
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
#[crate::openapi::path(
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
#[crate::openapi::path(
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
#[crate::openapi::path(
    get,
    path = "/v1/me/telegram",
    operation_id = "get_my_telegram_status",
    tag = "me",
    responses(
        (status = 200, description = "Link status per bot", body = TelegramStatus),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn telegram(
    State(state): State<AppState>,
    crate::auth::ShopOrEmployeeUser(user): crate::auth::ShopOrEmployeeUser,
) -> ApiResult<Json<TelegramStatus>> {
    Ok(Json(telegram_status(&state, user.id).await?))
}

/// The user's link to each bot, checked with Telegram (a rejected link is
/// removed and reported as not linked).
pub async fn telegram_status(
    state: &AppState,
    user_id: i32,
) -> Result<TelegramStatus, sqlx::Error> {
    let ids = telegram_ids(&state.pool, user_id).await?;
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
        clear_telegram_id(&state.pool, user_id, Bot::Leads).await?;
        status.leads_linked = false;
    }
    if notifications_ok == Some(false) {
        clear_telegram_id(&state.pool, user_id, Bot::Notifications).await?;
        status.notifications_linked = false;
    }
    Ok(status)
}

/// Body of `PUT /v1/me/sidebar-pin`.
#[derive(Debug, serde::Deserialize, crate::openapi::ToSchema)]
pub struct SidebarPin {
    /// Keep the desktop sidebar open instead of expanding on hover.
    pub pinned: bool,
}

/// Pin or unpin my sidebar
///
/// Saves whether the desktop sidebar stays open (`pined_bar` on `GET /v1/me`).
#[crate::openapi::path(
    put,
    path = "/v1/me/sidebar-pin",
    operation_id = "set_sidebar_pin",
    tag = "me",
    request_body = SidebarPin,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_sidebar_pin(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<SidebarPin>,
) -> ApiResult<axum::http::StatusCode> {
    sqlx::query("UPDATE users SET pined_bar = ? WHERE id = ?")
        .bind(input.pinned)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `positions.id` of an outside marketing agency.
const EXTERNAL_MARKETING_POSITION: i32 = 7;

/// A company the user does marketing for.
#[derive(Debug, serde::Serialize, sqlx::FromRow, crate::openapi::ToSchema)]
pub struct MarketingCompany {
    /// The company.
    pub id: i32,
    /// Its name.
    pub name: String,
}

/// List the companies I market for
///
/// Live companies where the signed-in user holds the External Marketing
/// position, for the marketing portal's company switcher.
#[crate::openapi::path(
    get,
    path = "/v1/me/marketing-companies",
    operation_id = "list_marketing_companies",
    tag = "me",
    responses(
        (status = 200, description = "Companies", body = [MarketingCompany]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn marketing_companies(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Vec<MarketingCompany>>> {
    let companies = sqlx::query_as(
        "SELECT c.id, c.name FROM company c
           JOIN users_positions up ON up.company_id = c.id
          WHERE up.user_id = ? AND up.position_id = ? AND c.deleted_at IS NULL
          ORDER BY c.name",
    )
    .bind(user.id)
    .bind(EXTERNAL_MARKETING_POSITION)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(companies))
}

/// Which Telegram alerts the user wants from the notifications bot.
#[derive(Debug, serde::Serialize, sqlx::FromRow, crate::openapi::ToSchema)]
pub struct NotificationPreferences {
    /// The notifications bot is linked.
    pub notifications_telegram_connected: bool,
    /// New texts.
    pub telegram_sms_notifications: bool,
    /// New emails.
    pub telegram_email_notifications: bool,
    /// Deal activities and other CRM alerts.
    pub telegram_activity_notifications: bool,
}

/// Body of `PUT /v1/me/notification-preferences`.
#[derive(Debug, serde::Deserialize, crate::openapi::ToSchema)]
pub struct NotificationPreferencesInput {
    /// New texts.
    pub telegram_sms_notifications: bool,
    /// New emails.
    pub telegram_email_notifications: bool,
    /// Deal activities and other CRM alerts.
    pub telegram_activity_notifications: bool,
}

async fn load_preferences(
    pool: &sqlx::MySqlPool,
    user_id: i32,
) -> ApiResult<NotificationPreferences> {
    let found: Option<NotificationPreferences> = sqlx::query_as(
        "SELECT notifications_telegram_id IS NOT NULL AS notifications_telegram_connected,
                COALESCE(telegram_sms_notifications, 0) <> 0 AS telegram_sms_notifications,
                COALESCE(telegram_email_notifications, 0) <> 0 AS telegram_email_notifications,
                COALESCE(telegram_activity_notifications, 0) <> 0 AS telegram_activity_notifications
           FROM users WHERE id = ? AND is_deleted = 0",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    found.ok_or(ApiError::NotFound)
}

/// Get my notification settings
///
/// Which alerts the notifications Telegram bot sends the signed-in user.
#[crate::openapi::path(
    get,
    path = "/v1/me/notification-preferences",
    operation_id = "get_notification_preferences",
    tag = "me",
    responses(
        (status = 200, description = "Settings", body = NotificationPreferences),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn notification_preferences(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<NotificationPreferences>> {
    Ok(Json(load_preferences(&state.pool, user.id).await?))
}

/// Save my notification settings
///
/// Turns each kind of Telegram alert on or off; returns the saved settings.
#[crate::openapi::path(
    put,
    path = "/v1/me/notification-preferences",
    operation_id = "set_notification_preferences",
    tag = "me",
    request_body = NotificationPreferencesInput,
    responses(
        (status = 200, description = "Saved settings", body = NotificationPreferences),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_notification_preferences(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<NotificationPreferencesInput>,
) -> ApiResult<Json<NotificationPreferences>> {
    sqlx::query(
        "UPDATE users SET telegram_sms_notifications = ?, telegram_email_notifications = ?,
                telegram_activity_notifications = ?
          WHERE id = ? AND is_deleted = 0",
    )
    .bind(input.telegram_sms_notifications)
    .bind(input.telegram_email_notifications)
    .bind(input.telegram_activity_notifications)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(Json(load_preferences(&state.pool, user.id).await?))
}

/// A Telegram id the widget sends as a number or a string.
#[derive(Debug, serde::Deserialize, crate::openapi::ToSchema)]
#[serde(untagged)]
pub enum TelegramNumber {
    Number(i64),
    Text(String),
}

impl TelegramNumber {
    fn text(&self) -> String {
        match self {
            Self::Number(number) => number.to_string(),
            Self::Text(text) => text.clone(),
        }
    }
}

/// Body of `POST /v1/me/telegram/{bot}`: what the Telegram login widget returned.
#[derive(Debug, serde::Deserialize, crate::openapi::ToSchema)]
pub struct TelegramLink {
    /// The CRM user the page linked for; must be the caller.
    pub crm_user_id: i32,
    /// Telegram user id.
    pub telegram_id: TelegramNumber,
    /// Unix seconds of the login.
    pub auth_date: TelegramNumber,
    /// Telegram's signature.
    pub hash: String,
    /// Telegram first name.
    pub first_name: String,
    /// Telegram last name.
    #[serde(default)]
    pub last_name: Option<String>,
    /// Telegram username.
    #[serde(default)]
    pub username: Option<String>,
    /// Telegram photo.
    #[serde(default)]
    pub photo_url: Option<String>,
}

async fn link_telegram(
    state: &AppState,
    user: &SessionUser,
    bot: Bot,
    input: &TelegramLink,
) -> ApiResult<axum::http::StatusCode> {
    use crate::telegram::{TelegramLogin, login_fresh, login_signed};
    let token = state
        .telegram
        .login_token(bot)
        .ok_or_else(|| ApiError::Internal("Telegram linking is not configured".into()))?;
    if input.crm_user_id != user.id {
        return Err(ApiError::Unauthorized);
    }
    let telegram_id = input.telegram_id.text();
    let auth_date = input.auth_date.text();
    if input.first_name.is_empty()
        || telegram_id.is_empty()
        || !telegram_id.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(ApiError::BadRequest("Invalid request".into()));
    }
    let fresh = auth_date
        .parse::<i64>()
        .is_ok_and(|at| login_fresh(at, chrono::Utc::now().timestamp()));
    if !fresh {
        return Err(ApiError::BadRequest(
            "Telegram authentication expired".into(),
        ));
    }
    let login = TelegramLogin {
        id: &telegram_id,
        auth_date: &auth_date,
        first_name: &input.first_name,
        last_name: input.last_name.as_deref(),
        username: input.username.as_deref(),
        photo_url: input.photo_url.as_deref(),
    };
    if !login_signed(&login, &input.hash, token) {
        return Err(ApiError::BadRequest("Invalid Telegram signature".into()));
    }
    let column = match bot {
        Bot::Leads => "telegram_id",
        Bot::Notifications => "notifications_telegram_id",
    };
    let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE users SET {column} = ?, temp_telegram_id = NULL, telegram_conf_code = NULL,
                telegram_conf_expires_at = NULL
          WHERE id = ? AND is_deleted = 0"
    )))
    .bind(&telegram_id)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Link my Telegram to the lead alerts bot
///
/// Checks the Telegram login widget's signature (made in the last day) and
/// saves the Telegram id for lead alerts.
#[crate::openapi::path(
    post,
    path = "/v1/me/telegram/leads",
    operation_id = "link_leads_telegram",
    tag = "me",
    request_body = TelegramLink,
    responses(
        (status = 204, description = "Linked"),
        (status = 400, description = "Bad, expired or forged login", body = ErrorBody),
        (status = 401, description = "No valid session, or another user's login", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Bot not configured, or database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn link_leads_telegram(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<TelegramLink>,
) -> ApiResult<axum::http::StatusCode> {
    link_telegram(&state, &user, Bot::Leads, &input).await
}

/// Link my Telegram to the notifications bot
///
/// Like the lead alerts link, for the CRM notifications bot; shop staff may
/// link it too.
#[crate::openapi::path(
    post,
    path = "/v1/me/telegram/notifications",
    operation_id = "link_notifications_telegram",
    tag = "me",
    request_body = TelegramLink,
    responses(
        (status = 204, description = "Linked"),
        (status = 400, description = "Bad, expired or forged login", body = ErrorBody),
        (status = 401, description = "No valid session, or another user's login", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or shop staff", body = ErrorBody),
        (status = 500, description = "Bot not configured, or database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn link_notifications_telegram(
    State(state): State<AppState>,
    crate::auth::ShopOrEmployeeUser(user): crate::auth::ShopOrEmployeeUser,
    JsonBody(input): JsonBody<TelegramLink>,
) -> ApiResult<axum::http::StatusCode> {
    link_telegram(&state, &user, Bot::Notifications, &input).await
}

/// Body of `POST /v1/me/onboarding-feedback`.
#[derive(Debug, serde::Deserialize, crate::openapi::ToSchema)]
pub struct OnboardingFeedback {
    /// 1 to 5 stars.
    pub rating: i32,
    /// Up to 2000 characters.
    #[serde(default)]
    pub feedback_text: Option<String>,
}

/// Rate the onboarding
///
/// Saves the signed-in user's rating of the guided tours; once per user.
#[crate::openapi::path(
    post,
    path = "/v1/me/onboarding-feedback",
    operation_id = "send_onboarding_feedback",
    tag = "me",
    request_body = OnboardingFeedback,
    responses(
        (status = 201, description = "Saved"),
        (status = 400, description = "Rating not 1-5 or text too long", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 409, description = "Feedback already submitted", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn onboarding_feedback(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<OnboardingFeedback>,
) -> ApiResult<axum::http::StatusCode> {
    let text = input
        .feedback_text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    if !(1..=5).contains(&input.rating) || text.is_some_and(|text| text.chars().count() > 2000) {
        return Err(ApiError::BadRequest("Invalid request".into()));
    }
    let given: Option<i32> =
        sqlx::query_scalar("SELECT id FROM onboarding_feedback WHERE user_id = ? LIMIT 1")
            .bind(user.id)
            .fetch_optional(&state.pool)
            .await?;
    if given.is_some() {
        return Err(ApiError::Conflict("Feedback already submitted".into()));
    }
    sqlx::query(
        "INSERT INTO onboarding_feedback (user_id, company_id, rating, feedback_text) VALUES (?, ?, ?, ?)",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(input.rating)
    .bind(text)
    .execute(&state.pool)
    .await?;
    Ok(axum::http::StatusCode::CREATED)
}
