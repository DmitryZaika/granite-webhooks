use chrono::{Duration, Utc};
use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;

use crate::crud::email_template::EmailTemplate;

pub struct ScheduledEmail {
    id: i32,
    template_body: String,
    customer_id: i32,
    email: String,
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
        SELECT scheduled_emails.id, template_body, customer_id, email
        FROM scheduled_emails
        JOIN users ON scheduled_emails.user_id = users.id
        JOIN email_templates ON scheduled_emails.template_id = email_templates.id
        WHERE send_at <= NOW() and sent_at IS NULL
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
        SET sent_at = NOW()
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
        let template_id = insert_test_template(&pool, "test_immediate_tpl", "Hello", Some(0)).await;
        let template = make_template(template_id, Some(0));

        insert_scheduled_email(&pool, template, 90001, 100, user_id, 1)
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
        assert_eq!(ready[0].customer_id, 100);
        assert_eq!(ready[0].template_body, "Hello");
    }

    /// Test that emails with hour_delay=None (defaults to 0) appear in ready.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_none_delay_defaults_to_zero(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_none@example.com", "Test None").await;
        let template_id = insert_test_template(&pool, "test_none_tpl", "Body", None).await;
        let template = make_template(template_id, None);

        insert_scheduled_email(&pool, template, 90002, 101, user_id, 1)
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
        let template_id =
            insert_test_template(&pool, "test_future_tpl", "Future body", Some(48)).await;
        let template = make_template(template_id, Some(48));

        insert_scheduled_email(&pool, template, 90003, 102, user_id, 1)
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

    /// Test that after marking an email as sent, it no longer appears in ready.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_mark_sent_removes_from_ready(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_mark@example.com", "Test Mark").await;
        let template_id = insert_test_template(&pool, "test_mark_tpl", "Mark me", Some(0)).await;
        let template = make_template(template_id, Some(0));

        insert_scheduled_email(&pool, template, 90004, 103, user_id, 1)
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
    }

    /// Test mixed scenario: some past, some future, some marked as sent.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_mixed_scenario(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_mixed@example.com", "Test Mixed").await;

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
        insert_scheduled_email(&pool, tpl_imm, 90010, 200, user_id, 1)
            .await
            .unwrap();
        insert_scheduled_email(&pool, tpl_fut, 90011, 201, user_id, 1)
            .await
            .unwrap();
        insert_scheduled_email(&pool, tpl_mark, 90012, 202, user_id, 1)
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

        // Now only 1 should be ready.
        let ready_after = get_ready_scheduled_emails(&pool).await.unwrap();
        assert_eq!(
            ready_after.len(),
            1,
            "only one email should be ready after marking one as sent"
        );
        assert_eq!(ready_after[0].template_body, "Immediate");
        assert_eq!(ready_after[0].customer_id, 200);
    }

    /// Test that email from get_ready includes the user's email via JOIN.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_ready_email_includes_user_email(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_email_join@example.com", "Join Test").await;
        let template_id = insert_test_template(&pool, "test_join_tpl", "Join body", Some(0)).await;
        let template = make_template(template_id, Some(0));

        insert_scheduled_email(&pool, template, 90020, 300, user_id, 1)
            .await
            .unwrap();

        // Allow send_at to become <= NOW() in MySQL.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let ready = get_ready_scheduled_emails(&pool).await.unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(
            ready[0].email, "test_email_join@example.com",
            "should include the user's email from the JOIN"
        );
    }

    /// Test that a negative hour_delay results in a past send_at, so the email appears ready.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_negative_delay_appears_in_ready(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_negdelay@example.com", "NegDelay Test").await;
        let template_id =
            insert_test_template(&pool, "test_negdelay_tpl", "Past body", Some(-1)).await;
        let template = make_template(template_id, Some(-1));

        // send_at = now - 1 hour, which is in the past.
        insert_scheduled_email(&pool, template, 90030, 400, user_id, 1)
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
        let template_id = insert_test_template(&pool, "test_double_tpl", "Double", Some(0)).await;
        let template = make_template(template_id, Some(0));

        insert_scheduled_email(&pool, template, 90040, 500, user_id, 1)
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
    }

    /// Test that multiple ready emails are all returned.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_multiple_ready_emails(pool: MySqlPool) {
        let user_id = insert_test_user(&pool, "test_multi@example.com", "Multi Test").await;

        let template_id1 = insert_test_template(&pool, "test_multi_tpl1", "First", Some(0)).await;
        let template_id2 = insert_test_template(&pool, "test_multi_tpl2", "Second", Some(0)).await;
        let template_id3 = insert_test_template(&pool, "test_multi_tpl3", "Third", Some(0)).await;

        insert_scheduled_email(
            &pool,
            make_template(template_id1, Some(0)),
            90050,
            501,
            user_id,
            1,
        )
        .await
        .unwrap();
        insert_scheduled_email(
            &pool,
            make_template(template_id2, Some(0)),
            90051,
            502,
            user_id,
            1,
        )
        .await
        .unwrap();
        insert_scheduled_email(
            &pool,
            make_template(template_id3, Some(0)),
            90052,
            503,
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

        let tpl_ready_id = insert_test_template(&pool, "test_fa_ready", "Ready one", Some(0)).await;
        let tpl_future_id =
            insert_test_template(&pool, "test_fa_future", "Future one", Some(24)).await;

        insert_scheduled_email(
            &pool,
            make_template(tpl_ready_id, Some(0)),
            90060,
            601,
            user_id,
            1,
        )
        .await
        .unwrap();
        insert_scheduled_email(
            &pool,
            make_template(tpl_future_id, Some(24)),
            90061,
            602,
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
}
