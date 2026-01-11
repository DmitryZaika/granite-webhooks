#[cfg(test)]
use crate::axum_helpers::axum_app::new_main_app;
#[cfg(test)]
use axum_test::TestServer;
use bytes::Bytes;
use sqlx::MySqlPool;
use std::fs;
use std::io;
use std::path::Path;

pub fn read_file_as_bytes<P: AsRef<Path>>(path: P) -> std::io::Result<Bytes> {
    let data = fs::read(path)?;
    Ok(Bytes::from(data))
}

pub async fn insert_user(
    pool: &MySqlPool,
    email: &str,
    telegram_id: Option<i64>,
) -> Result<u64, sqlx::Error> {
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

    Ok(rec.last_insert_id())
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
