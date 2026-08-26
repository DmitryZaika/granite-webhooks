use sqlx::MySqlPool;

#[derive(Debug)]
pub struct SalesUser {
    pub id: i32,
    pub telegram_id: Option<i64>,
    pub name: Option<String>,
    pub position_id: i32,
    pub mtd_lead_count: i64,
    pub user_position_id: i32,
}

pub struct UserTgInfo {
    pub telegram_id: Option<i64>,
    pub name: Option<String>,
    pub email: String,
}

pub struct UserNotificationsTgInfo {
    pub notifications_telegram_id: Option<i64>,
    pub telegram_sms_notifications: bool,
    pub telegram_email_notifications: bool,
    pub telegram_activity_notifications: bool,
}

pub async fn get_sales_users(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<SalesUser>, sqlx::Error> {
    sqlx::query_as!(
        SalesUser,
        r#"
        SELECT
            u.id,
            u.telegram_id,
            u.name,
            up.position_id,
            up.id as user_position_id,
            COUNT(c.id) as mtd_lead_count
        FROM users u
        INNER JOIN users_positions up ON u.id = up.user_id
        LEFT JOIN customers c ON u.id = c.sales_rep
            AND c.source = 'leads'
            AND c.assigned_date >= DATE_FORMAT(NOW(), '%Y-%m-01')
            AND c.company_id = u.company_id
            AND c.deleted_at IS NULL
        WHERE u.company_id = ?
        AND (up.position_id = 1 OR up.position_id = 2)
        GROUP BY u.id, u.telegram_id, u.name, up.position_id, user_position_id
        "#,
        company_id
    )
    .fetch_all(pool)
    .await
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

pub async fn get_user_notifications_tg_info(
    pool: &MySqlPool,
    user_id: i32,
) -> Result<Option<UserNotificationsTgInfo>, sqlx::Error> {
    sqlx::query_as!(
        UserNotificationsTgInfo,
        r#"
        SELECT
            notifications_telegram_id,
            telegram_sms_notifications as "telegram_sms_notifications!: bool",
            telegram_email_notifications as "telegram_email_notifications!: bool",
            telegram_activity_notifications as "telegram_activity_notifications!: bool"
        FROM users
        WHERE id = ?
        "#,
        user_id
    )
    .fetch_optional(pool)
    .await
}

pub async fn get_user_id_by_cloudtalk_agent(
    pool: &MySqlPool,
    company_id: i32,
    agent_id: &str,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT id
        FROM users
        WHERE company_id = ?
          AND cloudtalk_agent_id = ?
          AND is_deleted = 0
        LIMIT 1
        "#,
        company_id,
        agent_id
    )
    .fetch_optional(pool)
    .await
}

pub async fn get_user_id_by_ringcentral_agent(
    pool: &MySqlPool,
    company_id: i32,
    extension_id: &str,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar::<_, i32>(
        r#"
        SELECT id
        FROM users
        WHERE company_id = ?
          AND ringcentral_extension_id = ?
          AND is_deleted = 0
        LIMIT 1
        "#,
    )
    .bind(company_id)
    .bind(extension_id)
    .fetch_optional(pool)
    .await
}

pub async fn email_exists(pool: &MySqlPool, email: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM users
            WHERE email = ?
        ) AS "email_exists!: bool"
        "#,
        email
    )
    .fetch_one(pool)
    .await
}

pub async fn get_id_by_email(pool: &MySqlPool, email: &str) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar!(r#"SELECT id FROM users WHERE email = ?"#, email)
        .fetch_optional(pool)
        .await
}

/// Company that owns a user. Inbound mail derives `emails.company_id` from the
/// resolved receiver, which is the only party we can attribute with certainty.
pub async fn get_company_id_by_user_id(
    pool: &MySqlPool,
    user_id: i32,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar!(r#"SELECT company_id FROM users WHERE id = ?"#, user_id)
        .fetch_optional(pool)
        .await
}

/// Case-insensitive user lookup used when resolving recipient addresses.
/// `get_id_by_email` compares exactly; header addresses arrive in any case.
pub async fn get_id_by_email_normalized(
    pool: &MySqlPool,
    email: &str,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT id FROM users WHERE LOWER(TRIM(email)) = ? AND is_deleted = 0 LIMIT 1"#,
        email
    )
    .fetch_optional(pool)
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceivingEmail {
    To(i32),
    Forward(i32),
}

impl ReceivingEmail {
    pub const fn inner(self) -> i32 {
        match self {
            Self::To(value) | Self::Forward(value) => value,
        }
    }
}

pub async fn get_id_by_email_with_forward(
    pool: &MySqlPool,
    email: &str,
    forward: Option<&str>,
) -> Result<Option<ReceivingEmail>, sqlx::Error> {
    if let Some(user_id) = get_id_by_email(pool, email).await? {
        return Ok(Some(ReceivingEmail::To(user_id)));
    }
    let Some(inner_forward) = forward else {
        return Ok(None);
    };
    if let Some(user_id) = get_id_by_email(pool, inner_forward).await? {
        return Ok(Some(ReceivingEmail::Forward(user_id)));
    }
    Ok(None)
}
