use sqlx::MySqlPool;

#[derive(Debug)]
pub struct SalesUser {
    pub id: i32,
    pub telegram_id: Option<i64>,
    pub name: Option<String>,
    pub position_id: Option<i32>,
    pub mtd_lead_count: i64,
}

pub struct UserTgInfo {
    pub telegram_id: Option<i64>,
    pub name: Option<String>,
    pub email: String,
}

pub async fn get_sales_users(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<SalesUser>, sqlx::Error> {
    let users = sqlx::query_as!(
        SalesUser,
        r#"
        SELECT
            u.id,
            u.telegram_id,
            u.name,
            up.position_id,
            COUNT(c.id) as mtd_lead_count
        FROM users u
        INNER JOIN users_positions up ON u.id = up.user_id
        LEFT JOIN customers c ON u.id = c.sales_rep
            AND c.source = 'leads'
            AND c.assigned_date >= DATE_FORMAT(NOW(), '%Y-%m-01')
            AND c.company_id = u.company_id
        WHERE u.company_id = ?
        AND (up.position_id = 1 OR up.position_id = 2)
        GROUP BY u.id, u.telegram_id, u.name, up.position_id
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

pub async fn get_user_telegram_token(
    pool: &MySqlPool,
    telegram_id: i64,
) -> Result<Option<i32>, sqlx::Error> {
    let result = sqlx::query_scalar!(
        r#"SELECT telegram_conf_code FROM users WHERE temp_telegram_id = ?"#,
        telegram_id
    )
    .fetch_optional(pool)
    .await;

    match result {
        Ok(token) => Ok(token.flatten()),
        Err(e) => Err(e),
    }
}

pub async fn set_user_telegram_token(
    pool: &MySqlPool,
    telegram_id: i64,
    token: i32,
    email: &str,
) -> Result<(), sqlx::Error> {
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

pub async fn get_user_tg_info(
    pool: &MySqlPool,
    user_id: i32,
) -> Result<Option<UserTgInfo>, sqlx::Error> {
    sqlx::query_as!(
        UserTgInfo,
        r#"SELECT telegram_id, email, name FROM users WHERE id = ?"#,
        user_id
    )
    .fetch_optional(pool)
    .await
}
