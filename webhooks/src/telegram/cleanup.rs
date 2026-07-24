use crate::axum_helpers::guards::{RemixBackend, Telegram, TelegramBot};
use crate::crud::telegram_messages::{
    customer_belongs_to_company, list_active_telegram_lead_messages,
    mark_telegram_lead_message_deleted,
};
use crate::libs::constants::{ERR_DB, FORBIDDEN_RESPONSE, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use axum::extract::{Path, State};
use lambda_http::tracing;
use sqlx::MySqlPool;

pub async fn delete_lead_telegram_messages_inner<T>(
    pool: &MySqlPool,
    bot: &T,
    company_id: i32,
    customer_id: i32,
) -> BasicResponse
where
    T: Telegram + Send + Sync,
{
    let belongs = match customer_belongs_to_company(pool, company_id, customer_id).await {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                ?error,
                company_id = company_id,
                customer_id = customer_id,
                "Failed to verify customer ownership for telegram cleanup"
            );
            return internal_error(ERR_DB);
        }
    };
    if !belongs {
        return FORBIDDEN_RESPONSE;
    }

    let messages = match list_active_telegram_lead_messages(pool, company_id, customer_id).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(
                ?error,
                company_id = company_id,
                customer_id = customer_id,
                "Failed to load telegram lead messages"
            );
            return internal_error(ERR_DB);
        }
    };

    for message in messages {
        if let Err(error) = bot.delete_message(message.chat_id, message.message_id).await {
            tracing::error!(
                ?error,
                chat_id = message.chat_id,
                message_id = message.message_id,
                customer_id = customer_id,
                company_id = company_id,
                "Failed to delete telegram lead message"
            );
        }
        if let Err(error) = mark_telegram_lead_message_deleted(pool, message.id).await {
            tracing::error!(
                ?error,
                message_row_id = message.id,
                "Failed to mark telegram lead message deleted"
            );
        }
    }

    OK_RESPONSE
}

pub async fn delete_lead_telegram_messages(
    _: RemixBackend,
    State(pool): State<MySqlPool>,
    Path((company_id, customer_id)): Path<(i32, i32)>,
) -> BasicResponse {
    let bot = TelegramBot::default();
    delete_lead_telegram_messages_inner(&pool, &bot, company_id, customer_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crud::telegram_messages::insert_telegram_lead_message;
    use crate::libs::constants::SALES_MANAGER;
    use crate::tests::telegram::MockTelegram;
    use crate::tests::utils::positioned_user;
    use sqlx::MySqlPool;

    async fn insert_customer(pool: &MySqlPool, company_id: i32) -> i32 {
        let result = sqlx::query!(
            r#"
            INSERT INTO customers (name, phone, company_id, source)
            VALUES ('Lead', '+13179990000', ?, 'leads')
            "#,
            company_id
        )
        .execute(pool)
        .await
        .unwrap();
        i32::try_from(result.last_insert_id()).unwrap()
    }

    async fn active_message_count(pool: &MySqlPool, company_id: i32, customer_id: i32) -> i64 {
        sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS count
            FROM telegram_lead_messages
            WHERE company_id = ?
              AND customer_id = ?
              AND deleted_at IS NULL
            "#,
            company_id,
            customer_id
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn persists_manager_messages_for_new_lead(pool: MySqlPool) {
        use crate::telegram::send::send_telegram_manager_assign;

        let company_id = 1;
        let customer_id = insert_customer(&pool, company_id).await;
        let bot = MockTelegram::new();
        positioned_user(&pool, company_id, SALES_MANAGER, 456).await;
        positioned_user(&pool, company_id, SALES_MANAGER, 789).await;

        send_telegram_manager_assign(
            &pool,
            company_id,
            "New lead notification",
            u64::try_from(customer_id).unwrap(),
            true,
            &bot,
        )
        .await
        .unwrap();

        assert_eq!(active_message_count(&pool, company_id, customer_id).await, 2);
        assert_eq!(bot.sent.lock().unwrap().len(), 2);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn cleanup_deletes_all_tracked_messages(pool: MySqlPool) {
        let company_id = 1;
        let customer_id = insert_customer(&pool, company_id).await;
        let bot = MockTelegram::new();

        insert_telegram_lead_message(&pool, customer_id, company_id, 111, 10)
            .await
            .unwrap();
        insert_telegram_lead_message(&pool, customer_id, company_id, 222, 20)
            .await
            .unwrap();
        insert_telegram_lead_message(&pool, customer_id, company_id, 333, 30)
            .await
            .unwrap();

        let response =
            delete_lead_telegram_messages_inner(&pool, &bot, company_id, customer_id).await;
        assert_eq!(response, OK_RESPONSE);
        assert_eq!(active_message_count(&pool, company_id, customer_id).await, 0);

        let mut deleted = bot.deleted.lock().unwrap().clone();
        deleted.sort();
        assert_eq!(deleted, vec![(111, 10), (222, 20), (333, 30)]);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn cleanup_is_company_scoped(pool: MySqlPool) {
        let company_id = 1;
        let other_company_id = 2;
        let customer_id = insert_customer(&pool, company_id).await;
        let bot = MockTelegram::new();

        insert_telegram_lead_message(&pool, customer_id, company_id, 111, 10)
            .await
            .unwrap();

        let response =
            delete_lead_telegram_messages_inner(&pool, &bot, other_company_id, customer_id).await;
        assert_eq!(response, FORBIDDEN_RESPONSE);
        assert_eq!(active_message_count(&pool, company_id, customer_id).await, 1);
        assert!(bot.deleted.lock().unwrap().is_empty());
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn cleanup_succeeds_when_telegram_delete_fails(pool: MySqlPool) {
        let company_id = 1;
        let customer_id = insert_customer(&pool, company_id).await;
        let mut bot = MockTelegram::new();
        bot.fail_delete = true;

        insert_telegram_lead_message(&pool, customer_id, company_id, 111, 10)
            .await
            .unwrap();

        let response =
            delete_lead_telegram_messages_inner(&pool, &bot, company_id, customer_id).await;
        assert_eq!(response, OK_RESPONSE);
        assert_eq!(active_message_count(&pool, company_id, customer_id).await, 0);
        assert!(bot.deleted.lock().unwrap().is_empty());
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn sent_manager_notifications_are_deleted_on_cleanup(pool: MySqlPool) {
        use crate::crud::telegram_messages::list_active_telegram_lead_messages;
        use crate::telegram::send::send_telegram_manager_assign;

        let company_id = 1;
        let customer_id = insert_customer(&pool, company_id).await;
        let bot = MockTelegram::new();
        positioned_user(&pool, company_id, SALES_MANAGER, 456).await;
        positioned_user(&pool, company_id, SALES_MANAGER, 789).await;

        send_telegram_manager_assign(
            &pool,
            company_id,
            "New lead notification",
            u64::try_from(customer_id).unwrap(),
            true,
            &bot,
        )
        .await
        .unwrap();

        let tracked = list_active_telegram_lead_messages(&pool, company_id, customer_id)
            .await
            .unwrap();
        assert_eq!(tracked.len(), 2);

        let mut expected_deletes: Vec<(i64, i32)> = tracked
            .iter()
            .map(|message| (message.chat_id, message.message_id))
            .collect();
        expected_deletes.sort();

        let response =
            delete_lead_telegram_messages_inner(&pool, &bot, company_id, customer_id).await;
        assert_eq!(response, OK_RESPONSE);
        assert_eq!(active_message_count(&pool, company_id, customer_id).await, 0);

        let mut deleted = bot.deleted.lock().unwrap().clone();
        deleted.sort();
        assert_eq!(deleted, expected_deletes);
        assert!(
            deleted.iter().any(|entry| entry.0 == 456),
            "manager chat 456 notification must be deleted"
        );
        assert!(
            deleted.iter().any(|entry| entry.0 == 789),
            "manager chat 789 notification must be deleted"
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn sent_manager_and_sales_notifications_are_deleted_on_cleanup(pool: MySqlPool) {
        use crate::crud::telegram_messages::list_active_telegram_lead_messages;
        use crate::libs::constants::SALES_WORKER;
        use crate::telegram::send::{
            persist_lead_message, send_plain_message_to_chat, send_telegram_duplicate_notification,
        };

        let company_id = 1;
        let customer_id = insert_customer(&pool, company_id).await;
        let bot = MockTelegram::new();
        let sales_id = positioned_user(&pool, company_id, SALES_WORKER, 123).await;
        positioned_user(&pool, company_id, SALES_MANAGER, 456).await;
        positioned_user(&pool, company_id, SALES_MANAGER, 789).await;

        let sales_message =
            send_plain_message_to_chat(123, "You received a REPEATED lead Test", &bot)
                .await
                .unwrap();
        persist_lead_message(&pool, customer_id, company_id, &sales_message).await;

        send_telegram_duplicate_notification(
            &pool,
            company_id,
            customer_id,
            "Test",
            sales_id,
            "lead body".to_string(),
            &bot,
        )
        .await;

        let tracked = list_active_telegram_lead_messages(&pool, company_id, customer_id)
            .await
            .unwrap();
        assert_eq!(tracked.len(), 3);

        let mut expected_deletes: Vec<(i64, i32)> = tracked
            .iter()
            .map(|message| (message.chat_id, message.message_id))
            .collect();
        expected_deletes.sort();

        let response =
            delete_lead_telegram_messages_inner(&pool, &bot, company_id, customer_id).await;
        assert_eq!(response, OK_RESPONSE);
        assert_eq!(active_message_count(&pool, company_id, customer_id).await, 0);

        let mut deleted = bot.deleted.lock().unwrap().clone();
        deleted.sort();
        assert_eq!(deleted, expected_deletes);
        assert!(
            deleted.iter().any(|entry| entry.0 == 123),
            "sales notification must be deleted"
        );
        assert!(
            deleted.iter().any(|entry| entry.0 == 456),
            "manager notification must be deleted"
        );
        assert!(
            deleted.iter().any(|entry| entry.0 == 789),
            "second manager notification must be deleted"
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn cleanup_does_not_delete_other_customer_notifications(pool: MySqlPool) {
        use crate::telegram::send::send_telegram_manager_assign;

        let company_id = 1;
        let customer_id = insert_customer(&pool, company_id).await;
        let other_customer_id = insert_customer(&pool, company_id).await;
        let bot = MockTelegram::new();
        positioned_user(&pool, company_id, SALES_MANAGER, 456).await;

        send_telegram_manager_assign(
            &pool,
            company_id,
            "Lead A",
            u64::try_from(customer_id).unwrap(),
            true,
            &bot,
        )
        .await
        .unwrap();
        send_telegram_manager_assign(
            &pool,
            company_id,
            "Lead B",
            u64::try_from(other_customer_id).unwrap(),
            true,
            &bot,
        )
        .await
        .unwrap();

        assert_eq!(active_message_count(&pool, company_id, customer_id).await, 1);
        assert_eq!(
            active_message_count(&pool, company_id, other_customer_id).await,
            1
        );

        let response =
            delete_lead_telegram_messages_inner(&pool, &bot, company_id, customer_id).await;
        assert_eq!(response, OK_RESPONSE);
        assert_eq!(active_message_count(&pool, company_id, customer_id).await, 0);
        assert_eq!(
            active_message_count(&pool, company_id, other_customer_id).await,
            1
        );
        assert_eq!(bot.deleted.lock().unwrap().len(), 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn cleanup_works_after_customer_soft_delete(pool: MySqlPool) {
        let company_id = 1;
        let customer_id = insert_customer(&pool, company_id).await;
        let bot = MockTelegram::new();

        insert_telegram_lead_message(&pool, customer_id, company_id, 111, 10)
            .await
            .unwrap();

        sqlx::query!(
            r#"UPDATE customers SET deleted_at = NOW() WHERE id = ?"#,
            customer_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let response =
            delete_lead_telegram_messages_inner(&pool, &bot, company_id, customer_id).await;
        assert_eq!(response, OK_RESPONSE);
        assert_eq!(active_message_count(&pool, company_id, customer_id).await, 0);
        assert_eq!(bot.deleted.lock().unwrap().clone(), vec![(111, 10)]);
    }
}
