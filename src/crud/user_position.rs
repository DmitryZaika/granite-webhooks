use sqlx::MySqlPool;

#[derive(Debug)]
pub struct UserPosition {
    pub user_id: i32,
    pub company_id: i32,
}

pub async fn get_user_position(pool: &MySqlPool, id: i32) -> Result<UserPosition, sqlx::Error> {
    sqlx::query_as!(
        UserPosition,
        "SELECT user_id, company_id FROM users_positions WHERE id = ?",
        id
    )
    .fetch_one(pool)
    .await
}
