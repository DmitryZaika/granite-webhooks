use sqlx::MySqlPool;

pub struct UserData {
    pub name: Option<String>,
    pub email: Option<String>,
    pub phone_number: Option<String>,
    pub company_id: Option<i32>,
}

pub async fn get_user_template(pool: &MySqlPool, user_id: i32) -> Result<UserData, sqlx::Error> {
    sqlx::query_as!(
        UserData,
        r#"
        SELECT name, email, phone_number, company_id
        FROM users
        WHERE id = ?
        LIMIT 1
        "#,
        user_id
    )
    .fetch_one(pool)
    .await
}
