use chrono::{Duration, Utc};
use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;

use crate::crud::email_template::EmailTemplate;

pub struct ScheduledEmail {
    pub id: i32,
    pub template_body: String,
    pub template_subject: String,
    pub customer_id: i32,
    pub email: Option<String>,
    pub user_id: i32,
    pub deal_id: i32,
    pub company_id: i32,
}

pub async fn insert_scheduled_email(
    pool: &MySqlPool,
    template: EmailTemplate,
    deal_id: u64,
    customer_id: i32,
    user_id: i32,
    company_id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    let hour_delay: i64 = template.hour_delay.unwrap_or(0).into();
    let send_at = Utc::now() + Duration::hours(hour_delay);
    sqlx::query!(
        r#"
        INSERT INTO scheduled_emails (template_id, deal_id, customer_id, user_id, company_id, send_at)
        VALUES (?, ?, ?, ?, ?, ?)
        "#,
        template.id,
        deal_id,
        customer_id,
        user_id,
        company_id,
        send_at
    )
    .execute(pool)
    .await
}

pub async fn get_ready_scheduled_emails(
    pool: &MySqlPool,
) -> Result<Vec<ScheduledEmail>, sqlx::Error> {
    sqlx::query_as!(
        ScheduledEmail,
        r#"
        SELECT scheduled_emails.id, template_body, template_subject, customer_id, customers.email, scheduled_emails.user_id, scheduled_emails.deal_id, scheduled_emails.company_id
        FROM scheduled_emails
        JOIN customers ON scheduled_emails.customer_id = customers.id
        JOIN email_templates ON scheduled_emails.template_id = email_templates.id
        WHERE send_at <= UTC_TIMESTAMP() AND sent_at IS NULL AND status = 'pending'
        "#
    )
    .fetch_all(pool)
    .await
}

pub async fn mark_scheduled_email_as_sent(
    pool: &MySqlPool,
    id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE scheduled_emails
        SET sent_at = NOW(), status = 'sent'
        WHERE id = ?
        "#,
        id
    )
    .execute(pool)
    .await
}

pub async fn mark_scheduled_email_as_failed(
    pool: &MySqlPool,
    id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE scheduled_emails
        SET status = 'failed'
        WHERE id = ?
        "#,
        id
    )
    .execute(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::MySqlPool;
    use std::time::Duration;

    /// Helper: insert a user and return its id.
    async fn insert_test_user(pool: &MySqlPool, email: &str, name: &str) -> i32 {
        let result = sqlx::query!(
            "INSERT INTO users (email, name, company_id) VALUES (?, ?, 1)",
            email,
            name
        )
        .execute(pool)
        .await
        .expect("Failed to insert test user");
        result.last_insert_id() as i32
    }

    /// Helper: insert a customer and return its id.
    async fn insert_test_customer(
        pool: &MySqlPool,
        email: &str,
        name: &str,
        company_id: i32,
    ) -> i32 {
        let result = sqlx::query!(
            "INSERT INTO customers (email, name, company_id) VALUES (?, ?, ?)",
            email,
            name,
            company_id
        )
        .execute(pool)
        .await
        .expect("Failed to insert test customer");
        result.last_insert_id() as i32
    }

    /// Helper: insert an email template and return its id.
    async fn insert_test_template(
        pool: &MySqlPool,
        name: &str,
        body: &str,
        hour_delay: Option<i32>,
    ) -> i32 {
        let result = sqlx::query!(
            "INSERT INTO email_templates (template_name, template_body, company_id, hour_delay, show_template) VALUES (?, ?, 1, ?, 1)",
            name,
            body,
            hour_delay
        )
        .execute(pool)
        .await
        .expect("Failed to insert test template");
        result.last_insert_id() as i32
    }

    /// Helper: assemble an EmailTemplate struct for insert_scheduled_email.
    fn make_template(id: i32, hour_delay: Option<i32>) -> EmailTemplate {
        EmailTemplate { id, hour_delay }
    }

    /// Test that emails with hour_delay=0 appear in ready emails.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_immediate_send_appears_in_ready(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_immediate@example.com", "Test Immediate").await;
        let customer_id = insert_test_customer(&pool, "cust100@test.com", "Cust 100", 1).await;
        let template_id = insert_test_template(&pool, "test_immediate_tpl", "Hello", Some(0)).await;
        let template = make_template(template_id, Some(0));

        insert_scheduled_email(&pool, template, 90001, customer_id, user_id, 1)
            .await
            .expect("insert should succeed");

        // Allow send_at to become <= NOW() in MySQL.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let ready = get_ready_scheduled_emails(&pool)
            .await
            .expect("query should succeed");

        assert_eq!(
            ready.len(),
            1,
            "email with hour_delay=0 should appear in ready"
        );
        assert_eq!(ready[0].customer_id, customer_id);
        assert_eq!(ready[0].template_body, "Hello");
    }

    /// Test that emails with hour_delay=None (defaults to 0) appear in ready.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_none_delay_defaults_to_zero(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_none@example.com", "Test None").await;
        let customer_id = insert_test_customer(&pool, "cust101@test.com", "Cust 101", 1).await;
        let template_id = insert_test_template(&pool, "test_none_tpl", "Body", None).await;
        let template = make_template(template_id, None);

        insert_scheduled_email(&pool, template, 90002, customer_id, user_id, 1)
            .await
            .expect("insert should succeed");

        // Allow send_at to become <= NOW() in MySQL.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let ready = get_ready_scheduled_emails(&pool)
            .await
            .expect("query should succeed");

        assert_eq!(
            ready.len(),
            1,
            "email with hour_delay=None should default to 0 and appear in ready"
        );
    }

    /// Test that emails with a future hour_delay do NOT appear in ready emails.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_future_send_does_not_appear(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_future@example.com", "Test Future").await;
        let customer_id = insert_test_customer(&pool, "cust102@test.com", "Cust 102", 1).await;
        let template_id =
            insert_test_template(&pool, "test_future_tpl", "Future body", Some(48)).await;
        let template = make_template(template_id, Some(48));

        insert_scheduled_email(&pool, template, 90003, customer_id, user_id, 1)
            .await
            .expect("insert should succeed");

        let ready = get_ready_scheduled_emails(&pool)
            .await
            .expect("query should succeed");

        // The email should NOT appear because send_at is 48 hours in the future.
        assert!(
            ready.is_empty(),
            "email with hour_delay=48 should NOT appear in ready"
        );
    }

    /// Test that after marking an email as sent, it no longer appears in ready and status is 'sent'.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_mark_sent_removes_from_ready(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_mark@example.com", "Test Mark").await;
        let customer_id = insert_test_customer(&pool, "cust103@test.com", "Cust 103", 1).await;
        let template_id = insert_test_template(&pool, "test_mark_tpl", "Mark me", Some(0)).await;
        let template = make_template(template_id, Some(0));

        insert_scheduled_email(&pool, template, 90004, customer_id, user_id, 1)
            .await
            .expect("insert should succeed");

        // Allow send_at to become <= NOW() in MySQL.
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Should appear in ready.
        let ready = get_ready_scheduled_emails(&pool).await.unwrap();
        assert_eq!(ready.len(), 1);
        let email_id = ready[0].id;

        // Mark as sent.
        mark_scheduled_email_as_sent(&pool, email_id)
            .await
            .expect("mark as sent should succeed");

        // Should no longer appear.
        let ready_after = get_ready_scheduled_emails(&pool).await.unwrap();
        assert!(
            ready_after.is_empty(),
            "after marking as sent, email should not appear in ready"
        );

        // Verify that status is now 'sent'.
        let row = sqlx::query!("SELECT status FROM scheduled_emails WHERE id = ?", email_id)
            .fetch_one(&pool)
            .await
            .expect("should fetch the marked email");
        assert_eq!(
            row.status, "sent",
            "status should be 'sent' after marking as sent"
        );
    }

    /// Test mixed scenario: some past, some future, some marked as sent.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_mixed_scenario(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_mixed@example.com", "Test Mixed").await;

        let cust200 = insert_test_customer(&pool, "cust200@test.com", "Cust 200", 1).await;
        let cust201 = insert_test_customer(&pool, "cust201@test.com", "Cust 201", 1).await;
        let cust202 = insert_test_customer(&pool, "cust202@test.com", "Cust 202", 1).await;

        // Template with immediate send.
        let tpl_imm_id = insert_test_template(&pool, "test_mixed_imm", "Immediate", Some(0)).await;
        let tpl_imm = make_template(tpl_imm_id, Some(0));

        // Template with future send.
        let tpl_fut_id = insert_test_template(&pool, "test_mixed_fut", "Future", Some(72)).await;
        let tpl_fut = make_template(tpl_fut_id, Some(72));

        // Template with immediate send (to be marked as sent).
        let tpl_mark_id = insert_test_template(&pool, "test_mixed_mark", "To Mark", Some(0)).await;
        let tpl_mark = make_template(tpl_mark_id, Some(0));

        // Insert all three.
        insert_scheduled_email(&pool, tpl_imm, 90010, cust200, user_id, 1)
            .await
            .unwrap();
        insert_scheduled_email(&pool, tpl_fut, 90011, cust201, user_id, 1)
            .await
            .unwrap();
        insert_scheduled_email(&pool, tpl_mark, 90012, cust202, user_id, 1)
            .await
            .unwrap();

        // Allow send_at to become <= NOW() in MySQL.
        tokio::time::sleep(Duration::from_secs(1)).await;

        // At this point, 2 should be ready (imm + mark).
        let ready_before = get_ready_scheduled_emails(&pool).await.unwrap();
        assert_eq!(
            ready_before.len(),
            2,
            "two immediate emails should be ready before marking"
        );

        // Find the one to mark and mark it.
        let to_mark = ready_before
            .iter()
            .find(|e| e.template_body == "To Mark")
            .expect("should find the 'To Mark' email");
        mark_scheduled_email_as_sent(&pool, to_mark.id)
            .await
            .unwrap();

        // Verify the marked email has status 'sent'.
        let row = sqlx::query!(
            "SELECT status FROM scheduled_emails WHERE id = ?",
            to_mark.id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.status, "sent");

        // Now only 1 should be ready.
        let ready_after = get_ready_scheduled_emails(&pool).await.unwrap();
        assert_eq!(
            ready_after.len(),
            1,
            "only one email should be ready after marking one as sent"
        );
        assert_eq!(ready_after[0].template_body, "Immediate");
        assert_eq!(ready_after[0].customer_id, cust200);
    }

    /// Test that email from get_ready includes the user's email via JOIN.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_ready_email_includes_user_email(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_email_join@example.com", "Join Test").await;
        let customer_id =
            insert_test_customer(&pool, "test_email_join@example.com", "Join Cust", 1).await;
        let template_id = insert_test_template(&pool, "test_join_tpl", "Join body", Some(0)).await;
        let template = make_template(template_id, Some(0));

        insert_scheduled_email(&pool, template, 90020, customer_id, user_id, 1)
            .await
            .unwrap();

        // Allow send_at to become <= NOW() in MySQL.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let ready = get_ready_scheduled_emails(&pool).await.unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(
            ready[0].email,
            Some("test_email_join@example.com".to_string()),
            "should include the customer's email from the JOIN"
        );
    }

    /// Test that a negative hour_delay results in a past send_at, so the email appears ready.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_negative_delay_appears_in_ready(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_negdelay@example.com", "NegDelay Test").await;
        let customer_id = insert_test_customer(&pool, "cust400@test.com", "Cust 400", 1).await;
        let template_id =
            insert_test_template(&pool, "test_negdelay_tpl", "Past body", Some(-1)).await;
        let template = make_template(template_id, Some(-1));

        // send_at = now - 1 hour, which is in the past.
        insert_scheduled_email(&pool, template, 90030, customer_id, user_id, 1)
            .await
            .unwrap();

        let ready = get_ready_scheduled_emails(&pool).await.unwrap();
        assert_eq!(
            ready.len(),
            1,
            "email with negative hour_delay (past send_at) should appear"
        );
    }

    /// Test that marking a non-existent email id does not crash and affects 0 rows.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_mark_sent_nonexistent_id(pool: MySqlPool) {
        let result = mark_scheduled_email_as_sent(&pool, -1).await;
        assert!(result.is_ok(), "marking a non-existent id should not error");
        assert_eq!(
            result.unwrap().rows_affected(),
            0,
            "marking a non-existent id should affect 0 rows"
        );
    }

    /// Test that an email already marked as sent is not returned again after re-marking.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_double_mark_sent(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_double@example.com", "Double Mark").await;
        let customer_id = insert_test_customer(&pool, "cust500@test.com", "Cust 500", 1).await;
        let template_id = insert_test_template(&pool, "test_double_tpl", "Double", Some(0)).await;
        let template = make_template(template_id, Some(0));

        insert_scheduled_email(&pool, template, 90040, customer_id, user_id, 1)
            .await
            .unwrap();

        // Allow send_at to become <= NOW() in MySQL.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let ready = get_ready_scheduled_emails(&pool).await.unwrap();
        let email_id = ready[0].id;

        // First mark.
        mark_scheduled_email_as_sent(&pool, email_id).await.unwrap();

        // Second mark — should succeed but affect 1 row (it updates the same row again).
        let result = mark_scheduled_email_as_sent(&pool, email_id).await.unwrap();
        assert_eq!(result.rows_affected(), 1);

        // Still not in ready.
        let ready_after = get_ready_scheduled_emails(&pool).await.unwrap();
        assert!(ready_after.is_empty());

        // Verify status remains 'sent' after double-mark.
        let row = sqlx::query!("SELECT status FROM scheduled_emails WHERE id = ?", email_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.status, "sent");
    }

    /// Test that multiple ready emails are all returned.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_multiple_ready_emails(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_multi@example.com", "Multi Test").await;

        let cust501 = insert_test_customer(&pool, "cust501@test.com", "Cust 501", 1).await;
        let cust502 = insert_test_customer(&pool, "cust502@test.com", "Cust 502", 1).await;
        let cust503 = insert_test_customer(&pool, "cust503@test.com", "Cust 503", 1).await;

        let template_id1 = insert_test_template(&pool, "test_multi_tpl1", "First", Some(0)).await;
        let template_id2 = insert_test_template(&pool, "test_multi_tpl2", "Second", Some(0)).await;
        let template_id3 = insert_test_template(&pool, "test_multi_tpl3", "Third", Some(0)).await;

        insert_scheduled_email(
            &pool,
            make_template(template_id1, Some(0)),
            90050,
            cust501,
            user_id,
            1,
        )
        .await
        .unwrap();
        insert_scheduled_email(
            &pool,
            make_template(template_id2, Some(0)),
            90051,
            cust502,
            user_id,
            1,
        )
        .await
        .unwrap();
        insert_scheduled_email(
            &pool,
            make_template(template_id3, Some(0)),
            90052,
            cust503,
            user_id,
            1,
        )
        .await
        .unwrap();

        // Allow send_at to become <= NOW() in MySQL.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let ready = get_ready_scheduled_emails(&pool).await.unwrap();
        assert_eq!(ready.len(), 3);

        let bodies: Vec<&str> = ready.iter().map(|e| e.template_body.as_str()).collect();
        assert!(bodies.contains(&"First"));
        assert!(bodies.contains(&"Second"));
        assert!(bodies.contains(&"Third"));
    }

    /// Test that a future email stays hidden even with other ready emails present.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_future_among_ready(pool: MySqlPool) {
        let user_id =
            insert_test_user(&pool, "test_future_among@example.com", "Future Among").await;

        let cust601 = insert_test_customer(&pool, "cust601@test.com", "Cust 601", 1).await;
        let cust602 = insert_test_customer(&pool, "cust602@test.com", "Cust 602", 1).await;

        let tpl_ready_id = insert_test_template(&pool, "test_fa_ready", "Ready one", Some(0)).await;
        let tpl_future_id =
            insert_test_template(&pool, "test_fa_future", "Future one", Some(24)).await;

        insert_scheduled_email(
            &pool,
            make_template(tpl_ready_id, Some(0)),
            90060,
            cust601,
            user_id,
            1,
        )
        .await
        .unwrap();
        insert_scheduled_email(
            &pool,
            make_template(tpl_future_id, Some(24)),
            90061,
            cust602,
            user_id,
            1,
        )
        .await
        .unwrap();

        // Allow send_at to become <= NOW() in MySQL.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let ready = get_ready_scheduled_emails(&pool).await.unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].template_body, "Ready one");
    }

    /// Test that mark_scheduled_email_as_failed sets status to 'failed' and does NOT set sent_at.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_mark_as_failed(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_failed@example.com", "Test Failed").await;
        let customer_id = insert_test_customer(&pool, "cust700@test.com", "Cust 700", 1).await;
        let template_id = insert_test_template(&pool, "test_failed_tpl", "Fail me", Some(0)).await;
        let template = make_template(template_id, Some(0));

        insert_scheduled_email(&pool, template, 90070, customer_id, user_id, 1)
            .await
            .unwrap();

        // Allow send_at to become <= NOW() in MySQL.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let ready = get_ready_scheduled_emails(&pool).await.unwrap();
        assert_eq!(ready.len(), 1);
        let email_id = ready[0].id;

        // Mark as failed.
        mark_scheduled_email_as_failed(&pool, email_id)
            .await
            .unwrap();

        // Verify status is 'failed' and sent_at is still NULL.
        let row = sqlx::query!(
            "SELECT status, sent_at FROM scheduled_emails WHERE id = ?",
            email_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.status, "failed");
        assert!(
            row.sent_at.is_none(),
            "sent_at should remain NULL when marking as failed"
        );

        // Should no longer appear in ready (status is not 'pending').
        let ready_after = get_ready_scheduled_emails(&pool).await.unwrap();
        assert!(ready_after.is_empty());
    }

    /// Test that marking a non-existent email as failed does not crash.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_mark_failed_nonexistent_id(pool: MySqlPool) {
        let result = mark_scheduled_email_as_failed(&pool, -1).await;
        assert!(result.is_ok(), "marking a non-existent id should not error");
        assert_eq!(result.unwrap().rows_affected(), 0);
    }
}
