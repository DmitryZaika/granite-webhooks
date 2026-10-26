use crate::openapi::ToSchema;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// The signed-in user's monthly sales goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct SalesGoal {
    /// Goal for each month, decimal string in USD (e.g. "50000.00"); null
    /// when the user has not set one (the dashboard then assumes $50,000).
    pub monthly_sales_goal: Option<String>,
}

/// A new monthly sales goal.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateSalesGoal {
    /// Goal for each month in USD, above 0 and below 10 billion; rounded to
    /// cents.
    pub monthly_sales_goal: f64,
}

/// Whether the signed-in user's Telegram is linked to each of the two bots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TelegramStatus {
    /// Linked to the lead alerts bot.
    pub leads_linked: bool,
    /// Linked to the CRM notifications bot.
    pub notifications_linked: bool,
    /// Telegram user id of the lead alerts bot, for the Telegram login
    /// widget; null when the bot is not configured.
    pub leads_bot_id: Option<i64>,
    /// Telegram user id of the notifications bot; null when not configured.
    pub notifications_bot_id: Option<i64>,
}

#[derive(Debug, FromRow)]
pub struct TelegramIdsRow {
    pub telegram_id: Option<String>,
    pub notifications_telegram_id: Option<String>,
}
