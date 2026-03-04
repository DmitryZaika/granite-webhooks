#[cfg(test)]
use crate::axum_helpers::axum_app::new_main_app;
#[cfg(test)]
use axum_test::TestServer;
use bytes::Bytes;
use sqlx::MySqlPool;
use std::fs;
use std::io;
use std::path::Path;
use uuid::Uuid;

pub fn read_file_as_bytes<P: AsRef<Path>>(path: P) -> std::io::Result<Bytes> {
    let data = fs::read(path)?;
    Ok(Bytes::from(data))
}

pub async fn insert_user(
    pool: &MySqlPool,
    email: &str,
    telegram_id: Option<i64>,
) -> Result<i32, sqlx::Error> {
    let rec = sqlx::query!(
        r#"
        INSERT INTO users (
            email,
            password,
            name,
            phone_number,
            is_employee,
            is_admin,
            is_superuser,
            is_deleted,
            company_id,
            telegram_id,
            telegram_conf_code,
            telegram_conf_expires_at,
            temp_telegram_id
        )
        VALUES (?, NULL, NULL, NULL, false, false, false, false, 1, ?, NULL, NULL, NULL)
        "#,
        email,
        telegram_id
    )
    .execute(pool)
    .await?;

    Ok(rec.last_insert_id().try_into().unwrap())
}

pub fn replace_bytes(input: &[u8], search: &str, replace_with: &str) -> io::Result<Bytes> {
    let s =
        std::str::from_utf8(input).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let replaced = s.replace(search, replace_with);

    Ok(Bytes::from(replaced.into_bytes()))
}

#[cfg(test)]
pub fn new_test_app(pool: MySqlPool) -> TestServer {
    let app = new_main_app(pool);
    TestServer::builder().build(app).unwrap()
}

pub async fn assigned_user_position(
    pool: &MySqlPool,
    company_id: i32,
    position_id: i32,
    user_id: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO positions (id, name)
        VALUES (?, 'Sales')
        ON DUPLICATE KEY UPDATE
            name = VALUES(name)
        "#,
        position_id
    )
    .execute(pool)
    .await?;

    sqlx::query!(
        r#"
        INSERT INTO users_positions (user_id, position_id, company_id)
        VALUES (?, ?, ?)
        ON DUPLICATE KEY UPDATE
            position_id = VALUES(position_id)
        "#,
        user_id,
        position_id,
        company_id
    )
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn positioned_user(
    pool: &MySqlPool,
    company_id: i32,
    position_id: i32,
    telegram_id: i64,
) -> i32 {
    let email = format!("user_{}_email@example.com", Uuid::new_v4());
    let sales_id = insert_user(pool, &email, Some(telegram_id)).await.unwrap();
    assigned_user_position(pool, company_id, position_id, sales_id)
        .await
        .unwrap();
    return sales_id;
}
