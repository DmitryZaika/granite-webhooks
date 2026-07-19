use sqlx::mysql::MySqlQueryResult;
use sqlx::{MySqlPool, query};

pub struct TelegramLeadMessage {
    pub id: u64,
    pub chat_id: i64,
    pub message_id: i32,
}

pub async fn insert_telegram_lead_message(
    pool: &MySqlPool,
    customer_id: i32,
    company_id: i32,
    chat_id: i64,
    message_id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    query!(
        r#"
        INSERT INTO telegram_lead_messages (customer_id, company_id, chat_id, message_id)
        VALUES (?, ?, ?, ?)
        "#,
        customer_id,
        company_id,
        chat_id,
        message_id
    )
    .execute(pool)
    .await
}

pub async fn list_active_telegram_lead_messages(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> Result<Vec<TelegramLeadMessage>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT id, chat_id, message_id
        FROM telegram_lead_messages
        WHERE company_id = ?
          AND customer_id = ?
          AND deleted_at IS NULL
        "#,
        company_id,
        customer_id
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| TelegramLeadMessage {
            id: row.id,
            chat_id: row.chat_id,
            message_id: row.message_id,
        })
        .collect())
}

pub async fn list_active_manager_telegram_lead_messages(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> Result<Vec<TelegramLeadMessage>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT DISTINCT tlm.id, tlm.chat_id, tlm.message_id
        FROM telegram_lead_messages tlm
        INNER JOIN users u
            ON u.telegram_id = tlm.chat_id
            AND u.company_id = tlm.company_id
        INNER JOIN users_positions up
            ON up.user_id = u.id
            AND up.position_id = 2
        WHERE tlm.company_id = ?
          AND tlm.customer_id = ?
          AND tlm.deleted_at IS NULL
        "#,
        company_id,
        customer_id
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| TelegramLeadMessage {
            id: row.id,
            chat_id: row.chat_id,
            message_id: row.message_id,
        })
        .collect())
}

pub async fn mark_telegram_lead_message_deleted(
    pool: &MySqlPool,
    id: u64,
) -> Result<MySqlQueryResult, sqlx::Error> {
    query!(
        r#"
        UPDATE telegram_lead_messages
        SET deleted_at = NOW()
        WHERE id = ?
          AND deleted_at IS NULL
        "#,
        id
    )
    .execute(pool)
    .await
}

pub async fn customer_belongs_to_company(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query_scalar!(
        r#"
        SELECT id
        FROM customers
        WHERE id = ?
          AND company_id = ?
        LIMIT 1
        "#,
        customer_id,
        company_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::constants::{SALES_MANAGER, SALES_WORKER};
    use crate::tests::utils::positioned_user;
    use sqlx::MySqlPool;

    async fn insert_customer(pool: &MySqlPool, company_id: i32, phone: &str) -> i32 {
        let result = sqlx::query!(
            r#"
            INSERT INTO customers (name, phone, company_id, source)
            VALUES ('Lead', ?, ?, 'leads')
            "#,
            phone,
            company_id
        )
        .execute(pool)
        .await
        .unwrap();
        i32::try_from(result.last_insert_id()).unwrap()
    }

    fn sorted_pairs(messages: &[TelegramLeadMessage]) -> Vec<(i64, i32)> {
        let mut pairs: Vec<(i64, i32)> = messages
            .iter()
            .map(|message| (message.chat_id, message.message_id))
            .collect();
        pairs.sort();
        pairs
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn identifies_only_sales_manager_messages(pool: MySqlPool) {
        let company_id = 1;
        let customer_id = insert_customer(&pool, company_id, "+13179990001").await;
        positioned_user(&pool, company_id, SALES_MANAGER, 456).await;
        positioned_user(&pool, company_id, SALES_MANAGER, 789).await;
        positioned_user(&pool, company_id, SALES_WORKER, 123).await;

        insert_telegram_lead_message(&pool, customer_id, company_id, 456, 10)
            .await
            .unwrap();
        insert_telegram_lead_message(&pool, customer_id, company_id, 789, 20)
            .await
            .unwrap();
        insert_telegram_lead_message(&pool, customer_id, company_id, 123, 30)
            .await
            .unwrap();
        insert_telegram_lead_message(&pool, customer_id, company_id, 999, 40)
            .await
            .unwrap();

        let managers = list_active_manager_telegram_lead_messages(&pool, company_id, customer_id)
            .await
            .unwrap();
        assert_eq!(sorted_pairs(&managers), vec![(456, 10), (789, 20)]);

        let all = list_active_telegram_lead_messages(&pool, company_id, customer_id)
            .await
            .unwrap();
        assert_eq!(all.len(), 4);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn excludes_soft_deleted_manager_messages(pool: MySqlPool) {
        let company_id = 1;
        let customer_id = insert_customer(&pool, company_id, "+13179990002").await;
        positioned_user(&pool, company_id, SALES_MANAGER, 456).await;
        positioned_user(&pool, company_id, SALES_MANAGER, 789).await;

        insert_telegram_lead_message(&pool, customer_id, company_id, 456, 10)
            .await
            .unwrap();
        let deleted = insert_telegram_lead_message(&pool, customer_id, company_id, 789, 20)
            .await
            .unwrap();
        mark_telegram_lead_message_deleted(&pool, deleted.last_insert_id())
            .await
            .unwrap();

        let managers = list_active_manager_telegram_lead_messages(&pool, company_id, customer_id)
            .await
            .unwrap();
        assert_eq!(sorted_pairs(&managers), vec![(456, 10)]);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn manager_identification_is_customer_scoped(pool: MySqlPool) {
        let company_id = 1;
        let customer_id = insert_customer(&pool, company_id, "+13179990003").await;
        let other_customer_id = insert_customer(&pool, company_id, "+13179990004").await;
        positioned_user(&pool, company_id, SALES_MANAGER, 456).await;

        insert_telegram_lead_message(&pool, customer_id, company_id, 456, 10)
            .await
            .unwrap();
        insert_telegram_lead_message(&pool, other_customer_id, company_id, 456, 20)
            .await
            .unwrap();

        let managers = list_active_manager_telegram_lead_messages(&pool, company_id, customer_id)
            .await
            .unwrap();
        assert_eq!(sorted_pairs(&managers), vec![(456, 10)]);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn manager_identification_requires_matching_company(pool: MySqlPool) {
        let company_id = 1;
        let other_company_id = i32::try_from(
            sqlx::query!(r#"INSERT INTO company (name) VALUES ('Other Co')"#)
                .execute(&pool)
                .await
                .unwrap()
                .last_insert_id(),
        )
        .unwrap();
        let customer_id = insert_customer(&pool, company_id, "+13179990005").await;
        positioned_user(&pool, company_id, SALES_MANAGER, 456).await;

        insert_telegram_lead_message(&pool, customer_id, company_id, 456, 10)
            .await
            .unwrap();
        insert_telegram_lead_message(&pool, customer_id, other_company_id, 456, 20)
            .await
            .unwrap();

        let managers = list_active_manager_telegram_lead_messages(&pool, company_id, customer_id)
            .await
            .unwrap();
        assert_eq!(sorted_pairs(&managers), vec![(456, 10)]);

        let other_company =
            list_active_manager_telegram_lead_messages(&pool, other_company_id, customer_id)
                .await
                .unwrap();
        assert!(other_company.is_empty());
    }
}
