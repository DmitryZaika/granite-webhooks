use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;
use uuid::Uuid;

use crate::amazon::email::extract_email_address;

pub struct OutboundScheduledEmail {
    pub scheduled_email_id: i32,
    pub user_id: i32,
    pub customer_id: i32,
    pub company_id: i32,
    pub deal_id: i32,
    pub subject: String,
    pub html_body: String,
    pub sender_from: String,
    pub recipient_email: String,
    pub message_id: String,
}

pub fn normalize_outbound_message_id(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches(['<', '>']).trim();
    if trimmed.is_empty() {
        return Uuid::new_v4().to_string();
    }
    if trimmed.len() <= 72 {
        return trimmed.to_string();
    }
    match trimmed.find('@') {
        Some(idx) if idx > 0 && idx <= 72 => trimmed[..idx].to_string(),
        _ => trimmed.chars().take(72).collect(),
    }
}

pub async fn record_outbound_scheduled_email(
    pool: &MySqlPool,
    email: &OutboundScheduledEmail,
) -> Result<u64, sqlx::Error> {
    let thread_id = Uuid::new_v4().to_string();
    let sender_email = extract_email_address(&email.sender_from);
    let receiver_email = extract_email_address(&email.recipient_email);
    let message_id = normalize_outbound_message_id(&email.message_id);

    let result = sqlx::query(
        r#"
        INSERT INTO emails (
            sender_user_id, subject, body, html_body, message_id,
            sender_email, receiver_email, thread_id, deal_id, company_id, sent_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())
        "#,
    )
    .bind(email.user_id)
    .bind(&email.subject)
    .bind(&email.html_body)
    .bind(&email.html_body)
    .bind(&message_id)
    .bind(&sender_email)
    .bind(&receiver_email)
    .bind(&thread_id)
    .bind(email.deal_id)
    .bind(email.company_id)
    .execute(pool)
    .await?;

    let email_id = result.last_insert_id();
    insert_outbound_participants(
        pool,
        email_id,
        &sender_email,
        &receiver_email,
        email.user_id,
        email.customer_id,
    )
    .await?;
    sqlx::query("UPDATE scheduled_emails SET message_id = ? WHERE id = ?")
        .bind(&message_id)
        .bind(email.scheduled_email_id)
        .execute(pool)
        .await?;

    Ok(email_id)
}

async fn insert_outbound_participants(
    pool: &MySqlPool,
    email_id: u64,
    sender_email: &str,
    receiver_email: &str,
    user_id: i32,
    customer_id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO email_participants
            (email_id, type, email, display_name, user_id, customer_id, position)
        VALUES
            (?, 'from', ?, NULL, ?, NULL, 0),
            (?, 'to', ?, NULL, NULL, ?, 0)
        "#,
    )
    .bind(email_id)
    .bind(sender_email)
    .bind(user_id)
    .bind(email_id)
    .bind(receiver_email)
    .bind(customer_id)
    .execute(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::MySqlPool;

    #[test]
    fn normalize_outbound_message_id_fits_column_and_strips_brackets() {
        assert_eq!(
            normalize_outbound_message_id(" <abc-123@email.amazonses.com> "),
            "abc-123@email.amazonses.com"
        );
        let long = format!("{}@email.amazonses.com", "a".repeat(60));
        let normalized = normalize_outbound_message_id(&format!("<{long}>"));
        assert_eq!(normalized.len(), 60);
        assert!(!normalized.contains('@'));
    }

    #[test]
    fn normalize_outbound_message_id_generates_id_when_ses_returns_empty() {
        let generated = normalize_outbound_message_id("   ");
        assert_eq!(generated.len(), 36);
        assert!(generated.contains('-'));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn records_automated_send_as_employee_message(pool: MySqlPool) {
        let user_id = sqlx::query("INSERT INTO users (email, name, company_id) VALUES (?, ?, 1)")
            .bind("dema@granitedepotindy.com")
            .bind("Dema")
            .execute(&pool)
            .await
            .unwrap()
            .last_insert_id() as i32;

        let customer_id = sqlx::query(
            "INSERT INTO customers (name, company_id, source) VALUES (?, 1, 'leads')",
        )
        .bind("Brian")
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_id() as i32;

        sqlx::query("INSERT INTO customers_emails (customer_id, email) VALUES (?, ?)")
            .bind(customer_id)
            .bind("brian@hughesproducts.com")
            .execute(&pool)
            .await
            .unwrap();

        let group_id = sqlx::query("INSERT INTO groups_list (name, company_id) VALUES (?, 1)")
            .bind("Leads")
            .execute(&pool)
            .await
            .unwrap()
            .last_insert_id() as i32;
        let list_id = sqlx::query(
            "INSERT INTO deals_list (name, group_id, position) VALUES (?, ?, 0)",
        )
        .bind("Not Contacted Yet")
        .bind(group_id)
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_id() as i32;
        let deal_id = sqlx::query(
            "INSERT INTO deals (customer_id, status, list_id, position, user_id) VALUES (?, 'Not Contacted Yet', ?, 0, ?)",
        )
        .bind(customer_id)
        .bind(list_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_id() as i32;

        let template_id = sqlx::query(
            "INSERT INTO email_templates (template_name, template_body, template_subject, company_id) VALUES (?, ?, ?, 1)",
        )
        .bind("Thank You for Your Request")
        .bind("<p>Hi Brian</p>")
        .bind("Thank You for Your Request")
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_id() as i32;

        let scheduled_id = sqlx::query(
            "INSERT INTO scheduled_emails (template_id, deal_id, customer_id, user_id, company_id, send_at, status) VALUES (?, ?, ?, ?, 1, UTC_TIMESTAMP(), 'pending')",
        )
        .bind(template_id)
        .bind(deal_id)
        .bind(customer_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_id() as i32;

        let email_id = record_outbound_scheduled_email(
            &pool,
            &OutboundScheduledEmail {
                scheduled_email_id: scheduled_id,
                user_id,
                customer_id,
                company_id: 1,
                deal_id,
                subject: "Thank You for Your Request".to_string(),
                html_body: "<p>Hi Brian, This is Dema with Granite Depot of Indianapolis.</p>"
                    .to_string(),
                sender_from: "\"Dema Granite Depot\" <dema@granitedepotindy.com>".to_string(),
                recipient_email: "brian@hughesproducts.com".to_string(),
                message_id: "0100018f-drip-test-000000@email.amazonses.com".to_string(),
            },
        )
        .await
        .expect("record should succeed");

        let row = sqlx::query!(
            r#"
            SELECT sender_user_id, subject, body, html_body, message_id, sender_email,
                   receiver_email, deal_id, company_id, thread_id
            FROM emails
            WHERE id = ?
            "#,
            email_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(row.sender_user_id, Some(user_id));
        assert_eq!(row.subject.as_deref(), Some("Thank You for Your Request"));
        assert_eq!(
            row.sender_email.as_deref(),
            Some("dema@granitedepotindy.com")
        );
        assert_eq!(
            row.receiver_email.as_deref(),
            Some("brian@hughesproducts.com")
        );
        assert_eq!(row.deal_id, Some(deal_id as u64));
        assert_eq!(row.company_id, Some(1));
        assert!(row.thread_id.as_ref().is_some_and(|id| id.len() == 36));
        assert_eq!(
            row.message_id.as_deref(),
            Some("0100018f-drip-test-000000@email.amazonses.com")
        );
        assert!(
            row.html_body
                .as_deref()
                .is_some_and(|body| body.contains("Hi Brian"))
        );

        let types: Vec<String> = sqlx::query_scalar(
            "SELECT type FROM email_participants WHERE email_id = ? ORDER BY type",
        )
        .bind(email_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(types, vec!["from".to_string(), "to".to_string()]);

        let stored_message_id: Option<String> = sqlx::query_scalar(
            "SELECT message_id FROM scheduled_emails WHERE id = ?",
        )
        .bind(scheduled_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            stored_message_id.as_deref(),
            Some("0100018f-drip-test-000000@email.amazonses.com")
        );
    }
}
