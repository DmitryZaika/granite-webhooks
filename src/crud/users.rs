use sqlx::MySqlPool;

#[derive(Debug)]
pub struct SalesUser {
    pub id: i32,
    pub telegram_id: Option<i64>, // BIGINT in DB recommended
    pub name: Option<String>,
    pub position_id: Option<i32>, // match INT column
}

pub async fn get_sales_users(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<SalesUser>, sqlx::Error> {
    let users = sqlx::query_as!(
        SalesUser,
        r#"
        SELECT 
            id,
            telegram_id,
            name,
            position_id
        FROM users 
        WHERE company_id = ? 
        AND position_id = 1 OR position_id = 2
        "#,
        company_id
    )
    .fetch_all(pool)
    .await?;

    Ok(users)
}


pub async fn set_telegram_id(pool: &MySqlPool, email: &str, telegram_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE users SET telegram_id = ? WHERE email = ?
        "#,
        telegram_id,
        email
    )
    .execute(pool)
    .await?;

    Ok(())
}