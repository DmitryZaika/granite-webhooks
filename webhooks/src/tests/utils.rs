use crate::amazon::bucket::S3Bucket;
#[cfg(test)]
use crate::axum_helpers::axum_app::new_main_app;
#[cfg(test)]
use axum_test::TestServer;
use bytes::Bytes;
use sqlx::MySqlPool;
#[cfg(test)]
use sqlx::mysql::MySqlQueryResult;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone)]
pub struct MockClient {
    pub path: PathBuf,
}

impl MockClient {
    pub fn new<P: Into<PathBuf>>(path: P) -> Self {
        Self { path: path.into() }
    }
}

impl S3Bucket for MockClient {
    async fn read_bytes(&self, _bucket: &str, _key: &str) -> Result<Bytes, String> {
        read_file_as_bytes(&self.path).map_err(|e| e.to_string())
    }
    fn send_file<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
        data: Bytes,
    ) -> impl Future<Output = Result<String, String>> + Send + 'a {
        async move { Ok(format!("s3://{bucket}/{key}")) }
    }
}

pub struct Email {
    pub id: i32,
    pub receiver_user_id: Option<i32>,
    pub receiver_email: Option<String>,
    pub sender_user_id: Option<i32>,
    pub subject: Option<String>,
    pub body: Option<String>,
    pub message_id: Option<String>,
    pub thread_id: Option<String>,
    pub bucket: Option<String>,
}
pub fn read_file_as_bytes<P: AsRef<Path>>(path: P) -> std::io::Result<Bytes> {
    let data = fs::read(path)?;
    Ok(Bytes::from(data))
}

#[cfg(test)]
pub async fn insert_email(
    pool: &MySqlPool,
    message_id: &str,
) -> Result<MySqlQueryResult, sqlx::Error> {
    let uuid: Uuid = Uuid::new_v4();
    sqlx::query!(
        r#"
        INSERT INTO emails (sender_user_id, subject, body, message_id, thread_id)
        VALUES (?, ?, ?, ?, ?)
        "#,
        1,
        "Test Subject",
        "Test Body",
        message_id,
        uuid.to_string()
    )
    .execute(pool)
    .await
}

#[cfg(test)]
pub async fn get_emails(pool: &MySqlPool) -> Result<Vec<Email>, sqlx::Error> {
    sqlx::query_as!(
        Email,
        r#"
        SELECT id, receiver_user_id, receiver_email, sender_user_id, subject, body, message_id, thread_id, bucket
        FROM emails
        ORDER BY id ASC
        LIMIT 10
        "#,
    )
    .fetch_all(pool)
    .await
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
    TestServer::builder().build(app)
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
    sales_id
}
