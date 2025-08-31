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


pub async fn set_telegram_id(pool: &MySqlPool, telegram_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE users SET telegram_id = ? WHERE temp_telegram_id = ?
        "#,
        telegram_id,
        telegram_id,
    )
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn user_has_telegram_id(pool: &MySqlPool, telegram_id: i64) -> Result<bool, sqlx::Error> {
    let user = sqlx::query_scalar!(
        r#"SELECT id FROM users WHERE telegram_id = ? "#,
        telegram_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(user.is_some())
}

pub async fn get_user_telegram_token(pool: &MySqlPool, telegram_id: i64) -> Result<Option<i32>, sqlx::Error> {
    let result =sqlx::query_scalar!(
        r#"SELECT telegram_conf_code FROM users WHERE temp_telegram_id = ?"#,
        telegram_id
    )
    .fetch_optional(pool)
    .await;

    match result {
        Ok(token) => Ok(token.flatten()),
        Err(e) => Err(e)
    }
}

pub async fn set_user_telegram_token(pool: &MySqlPool, telegram_id: i64, token: i32, email: &str) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"UPDATE users SET telegram_conf_code = ?, temp_telegram_id = ? WHERE email = ?"#,
        token,
        telegram_id,
        email
    )
    .execute(pool)
    .await?;
    Ok(())
}

