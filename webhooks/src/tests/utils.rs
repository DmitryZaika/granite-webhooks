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
    async fn send_file(&self, bucket: &str, key: &str, _data: Bytes) -> Result<String, String> {
        Ok(format!("s3://{bucket}/{key}"))
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailParticipantRow {
    pub email: String,
    pub display_name: Option<String>,
    pub user_id: Option<i32>,
    pub participant_type: String,
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
    let result = sqlx::query(
        r#"
        INSERT INTO emails (subject, body, message_id, thread_id)
        VALUES (?, ?, ?, ?)
        "#,
    )
    .bind("Test Subject")
    .bind("Test Body")
    .bind(message_id)
    .bind(uuid.to_string())
    .execute(pool)
    .await?;

    let email_id = result.last_insert_id();
    sqlx::query(
        r#"
        INSERT INTO email_participants (email_id, email, user_id, type)
        VALUES (?, ?, NULL, 'from')
        "#,
    )
    .bind(email_id)
    .bind("sender@example.com")
    .execute(pool)
    .await?;

    Ok(result)
}

#[cfg(test)]
pub async fn get_emails(pool: &MySqlPool) -> Result<Vec<Email>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT
            e.id AS id,
            (
                SELECT ep.user_id
                FROM email_participants ep
                WHERE ep.email_id = e.id
                  AND ep.type = 'to'
                  AND ep.user_id IS NOT NULL
                ORDER BY ep.id ASC
                LIMIT 1
            ) AS receiver_user_id,
            (
                SELECT ep.email
                FROM email_participants ep
                WHERE ep.email_id = e.id
                  AND ep.type = 'to'
                  AND ep.user_id IS NOT NULL
                ORDER BY ep.id ASC
                LIMIT 1
            ) AS receiver_email,
            (
                SELECT ep.user_id
                FROM email_participants ep
                WHERE ep.email_id = e.id
                  AND ep.type = 'from'
                ORDER BY ep.id ASC
                LIMIT 1
            ) AS sender_user_id,
            e.subject AS subject,
            e.body AS body,
            e.message_id AS message_id,
            e.thread_id AS thread_id,
            e.bucket AS bucket
        FROM emails e
        ORDER BY e.id ASC
        LIMIT 10
        "#,
    )
    .fetch_all(pool)
    .await?;

    let mut emails = Vec::with_capacity(rows.len());
    for row in rows {
        use sqlx::Row;
        emails.push(Email {
            id: row.try_get("id")?,
            receiver_user_id: row.try_get("receiver_user_id")?,
            receiver_email: row.try_get("receiver_email")?,
            sender_user_id: row.try_get("sender_user_id")?,
            subject: row.try_get("subject")?,
            body: row.try_get("body")?,
            message_id: row.try_get("message_id")?,
            thread_id: row.try_get("thread_id")?,
            bucket: row.try_get("bucket")?,
        });
    }
    Ok(emails)
}

#[cfg(test)]
pub async fn get_email_participants(
    pool: &MySqlPool,
    email_id: i32,
) -> Result<Vec<EmailParticipantRow>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT email, display_name, user_id, type AS participant_type
        FROM email_participants
        WHERE email_id = ?
        ORDER BY id ASC
        "#,
    )
    .bind(email_id)
    .fetch_all(pool)
    .await?;

    let mut participants = Vec::with_capacity(rows.len());
    for row in rows {
        use sqlx::Row;
        participants.push(EmailParticipantRow {
            email: row.try_get("email")?,
            display_name: row.try_get("display_name")?,
            user_id: row.try_get("user_id")?,
            participant_type: row.try_get("participant_type")?,
        });
    }
    Ok(participants)
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

#[cfg(test)]
pub async fn insert_group_list(pool: &MySqlPool, company_id: i32) -> Result<u64, sqlx::Error> {
    let rec = sqlx::query!(
        r#"INSERT INTO groups_list (name, company_id, is_default) VALUES ('Test Group', ?, 1)"#,
        company_id
    )
    .execute(pool)
    .await?;
    Ok(rec.last_insert_id())
}

#[cfg(test)]
pub async fn insert_deals_list(pool: &MySqlPool, group_id: u64) -> Result<u64, sqlx::Error> {
    let rec = sqlx::query!(
        r#"INSERT INTO deals_list (name, group_id, position) VALUES ('Test Deals List', ?, 0)"#,
        group_id,
    )
    .execute(pool)
    .await?;
    Ok(rec.last_insert_id())
}
