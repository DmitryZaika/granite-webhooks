use serde::Serialize;
use sqlx::{FromRow, MySqlPool};
use utoipa::ToSchema;

/// `positions.id` of the sales rep position (`Positions.SalesRep` in Remix).
pub const SALES_REP_POSITION_ID: i32 = 1;

/// A user's id and display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct UserNameRow {
    /// User id.
    pub id: i32,
    /// Display name.
    pub name: Option<String>,
}

/// Remix: `api.sales-reps.tsx`. Like Remix, the position row is not filtered
/// by `up.company_id`, only the user's own `company_id` is.
pub async fn sales_reps_in_company(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<UserNameRow>, sqlx::Error> {
    sqlx::query_as::<_, UserNameRow>(
        r"
        SELECT u.id, u.name
          FROM users u
          JOIN users_positions up ON up.user_id = u.id
         WHERE up.position_id = ?
           AND u.is_deleted = 0
           AND u.company_id = ?
        ",
    )
    .bind(SALES_REP_POSITION_ID)
    .bind(company_id)
    .fetch_all(pool)
    .await
}
