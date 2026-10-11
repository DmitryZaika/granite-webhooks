use super::schemas::{SalesGoal, TelegramIdsRow};
use crate::telegram::Bot;
use serde::Serialize;
use sqlx::{FromRow, MySqlPool};
use utoipa::ToSchema;

/// A position the user holds in the current company.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct PositionRow {
    /// `positions.id`, e.g. 1 = sales rep, 9 = super admin.
    pub position_id: i32,
}

/// Remix: `customersListLoader.server.ts` positions query.
pub async fn positions_in_company(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
) -> Result<Vec<PositionRow>, sqlx::Error> {
    sqlx::query_as::<_, PositionRow>(
        r"
        SELECT up.position_id
          FROM users_positions up
         WHERE up.user_id = ? AND up.company_id = ?
        ",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_all(pool)
    .await
}

/// Remix: `loadStoredSalesRepMonthlyGoal`. A goal of 0 or less counts as unset.
pub async fn sales_goal(pool: &MySqlPool, user_id: i32) -> Result<SalesGoal, sqlx::Error> {
    let goal: Option<Option<String>> = sqlx::query_scalar(
        r"
        SELECT CASE WHEN monthly_sales_goal > 0 THEN CAST(monthly_sales_goal AS CHAR) END
          FROM users
         WHERE id = ? AND is_deleted = 0
        ",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(SalesGoal {
        monthly_sales_goal: goal.flatten(),
    })
}

/// Remix: `saveSalesRepMonthlyGoal`.
pub async fn save_sales_goal(
    pool: &MySqlPool,
    user_id: i32,
    amount: f64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE users SET monthly_sales_goal = ROUND(?, 2) WHERE id = ? AND is_deleted = 0",
    )
    .bind(amount)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The chat ids the user linked each bot to. Remix: `loadTelegramLinkStatus`.
pub async fn telegram_ids(pool: &MySqlPool, user_id: i32) -> Result<TelegramIdsRow, sqlx::Error> {
    let row = sqlx::query_as::<_, TelegramIdsRow>(
        r"
        SELECT CAST(telegram_id AS CHAR) AS telegram_id,
               CAST(notifications_telegram_id AS CHAR) AS notifications_telegram_id
          FROM users
         WHERE id = ? AND is_deleted = 0
        ",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or(TelegramIdsRow {
        telegram_id: None,
        notifications_telegram_id: None,
    }))
}

/// Forgets a link Telegram says no longer works, so the user is asked to
/// connect again.
pub async fn clear_telegram_id(
    pool: &MySqlPool,
    user_id: i32,
    bot: Bot,
) -> Result<(), sqlx::Error> {
    let sql = match bot {
        Bot::Leads => "UPDATE users SET telegram_id = NULL WHERE id = ? AND is_deleted = 0",
        Bot::Notifications => {
            "UPDATE users SET notifications_telegram_id = NULL WHERE id = ? AND is_deleted = 0"
        }
    };
    sqlx::query(sql).bind(user_id).execute(pool).await?;
    Ok(())
}
