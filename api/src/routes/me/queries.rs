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
