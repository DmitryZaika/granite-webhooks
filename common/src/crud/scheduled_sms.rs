use chrono::{Duration, Utc};
use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;

use crate::utils::email_send_window::clamp_automated_sms_send_at;

#[derive(sqlx::FromRow)]
struct SmsDripTemplate {
    id: i32,
    hour_delay: Option<i32>,
    minute_delay: i32,
}

pub async fn cancel_pending_scheduled_sms_for_deal(
    pool: &MySqlPool,
    deal_id: u64,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE scheduled_sms
        SET status = 'cancelled',
            error_message = 'Deal moved lists'
        WHERE deal_id = ?
          AND status = 'pending'
        "#,
    )
    .bind(deal_id)
    .execute(pool)
    .await
}

/// Queues the company SMS drips of `list_id` for the deal. A template goes to
/// the customer sources it names (none: only leads) and to deals of the sales
/// reps it names (none: every rep's), once per customer.
pub async fn schedule_sms_templates_for_deal_list(
    pool: &MySqlPool,
    list_id: i32,
    company_id: i32,
    deal_id: u64,
    customer_id: i32,
    user_id: i32,
) -> Result<(), sqlx::Error> {
    let source: Option<String> = sqlx::query_scalar(
        r#"SELECT LOWER(TRIM(IFNULL(source, ''))) FROM customers WHERE id = ? AND deleted_at IS NULL"#,
    )
    .bind(customer_id)
    .fetch_optional(pool)
    .await?;
    let Some(source) = source else {
        return Ok(());
    };

    let templates: Vec<SmsDripTemplate> = sqlx::query_as(
        r#"
        SELECT id, hour_delay, minute_delay
        FROM cloudtalk_sms_templates
        WHERE company_id = ?
          AND user_id IS NULL
          AND lead_list_id = ?
          AND deleted_at IS NULL
          AND IF(customer_sources IS NULL OR JSON_LENGTH(customer_sources) = 0,
                 ? = 'leads',
                 JSON_CONTAINS(customer_sources, JSON_QUOTE(?)))
          AND (sales_rep_ids IS NULL OR JSON_LENGTH(sales_rep_ids) = 0
               OR JSON_CONTAINS(sales_rep_ids, CAST(? AS JSON)))
        ORDER BY hour_delay ASC, id ASC
        "#,
    )
    .bind(company_id)
    .bind(list_id)
    .bind(&source)
    .bind(&source)
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    for template in templates {
        let existing: Option<i64> = sqlx::query_scalar(
            r#"
            SELECT CAST(id AS SIGNED) FROM scheduled_sms
            WHERE template_id = ?
              AND customer_id = ?
              AND status IN ('pending', 'sent')
            LIMIT 1
            "#,
        )
        .bind(template.id)
        .bind(customer_id)
        .fetch_optional(pool)
        .await?;
        if existing.is_some() {
            continue;
        }

        let send_at = clamp_automated_sms_send_at(
            Utc::now()
                + Duration::hours(template.hour_delay.unwrap_or(0).into())
                + Duration::minutes(template.minute_delay.into()),
        );
        sqlx::query(
            r#"
            INSERT INTO scheduled_sms (template_id, deal_id, list_id, customer_id, user_id, company_id, send_at, status)
            VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')
            "#,
        )
        .bind(template.id)
        .bind(deal_id)
        .bind(list_id)
        .bind(customer_id)
        .bind(user_id)
        .bind(company_id)
        .bind(send_at.naive_utc())
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::crud::scheduled_emails::schedule_templates_for_deal_list;
    use sqlx::MySqlPool;

    struct Setup {
        user_id: i32,
        list_id: i32,
    }

    async fn setup(pool: &MySqlPool) -> Setup {
        let user_id = sqlx::query(
            "INSERT INTO users (email, name, company_id) VALUES ('rep@sms.test', 'Rep', 1)",
        )
        .execute(pool)
        .await
        .unwrap()
        .last_insert_id() as i32;
        let group_id = sqlx::query("INSERT INTO groups_list (name, company_id) VALUES ('Leads', 1)")
            .execute(pool)
            .await
            .unwrap()
            .last_insert_id() as i32;
        let list_id = sqlx::query(
            "INSERT INTO deals_list (name, group_id, position) VALUES ('Not Contacted Yet', ?, 0)",
        )
        .bind(group_id)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_id() as i32;
        Setup { user_id, list_id }
    }

    async fn insert_template(
        pool: &MySqlPool,
        list_id: i32,
        minute_delay: i32,
        sources: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO cloudtalk_sms_templates
               (company_id, user_id, name, body, lead_list_id, hour_delay, minute_delay, customer_sources)
             VALUES (1, NULL, 'Drip', 'Hi', ?, 0, ?, ?)",
        )
        .bind(list_id)
        .bind(minute_delay)
        .bind(sources)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_customer_deal(pool: &MySqlPool, s: &Setup, source: &str) -> (i32, u64) {
        let customer_id = sqlx::query(
            "INSERT INTO customers (name, company_id, source, phone) VALUES ('Lead', 1, ?, '3175550100')",
        )
        .bind(source)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_id() as i32;
        let deal_id = sqlx::query(
            "INSERT INTO deals (customer_id, status, list_id, position, user_id) VALUES (?, 'Not Contacted Yet', ?, 0, ?)",
        )
        .bind(customer_id)
        .bind(s.list_id)
        .bind(s.user_id)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_id();
        (customer_id, deal_id)
    }

    async fn pending(pool: &MySqlPool, customer_id: i32) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM scheduled_sms WHERE customer_id = ? AND status = 'pending'",
        )
        .bind(customer_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[test_db::test]
    async fn new_lead_on_list_gets_drip_sms(pool: MySqlPool) {
        let s = setup(&pool).await;
        insert_template(&pool, s.list_id, 0, None).await;
        insert_template(&pool, s.list_id, 30, None).await;
        let (customer_id, deal_id) = insert_customer_deal(&pool, &s, "leads").await;

        schedule_templates_for_deal_list(
            &pool,
            s.list_id,
            1,
            deal_id,
            customer_id,
            s.user_id,
            true,
        )
        .await
        .unwrap();
        assert_eq!(pending(&pool, customer_id).await, 2);

        // Scheduling again (a repeated lead) does not queue the same texts twice.
        schedule_templates_for_deal_list(
            &pool,
            s.list_id,
            1,
            deal_id,
            customer_id,
            s.user_id,
            false,
        )
        .await
        .unwrap();
        assert_eq!(pending(&pool, customer_id).await, 2);
    }

    #[test_db::test]
    async fn drip_sms_follow_the_template_sources(pool: MySqlPool) {
        let s = setup(&pool).await;
        insert_template(&pool, s.list_id, 0, Some(r#"["check-in"]"#)).await;
        let (lead, lead_deal) = insert_customer_deal(&pool, &s, "leads").await;
        let (walk_in, walk_in_deal) = insert_customer_deal(&pool, &s, "check-in").await;

        for (customer_id, deal_id) in [(lead, lead_deal), (walk_in, walk_in_deal)] {
            schedule_templates_for_deal_list(
                &pool,
                s.list_id,
                1,
                deal_id,
                customer_id,
                s.user_id,
                true,
            )
            .await
            .unwrap();
        }
        assert_eq!(pending(&pool, lead).await, 0);
        assert_eq!(pending(&pool, walk_in).await, 1);
    }

    #[test_db::test]
    async fn drip_sms_follow_the_template_sales_reps(pool: MySqlPool) {
        let s = setup(&pool).await;
        let other_rep = s.user_id + 1000;
        for (minutes, reps) in [
            (0, format!("[{}]", s.user_id)),
            (10, format!("[{other_rep}]")),
            (20, "[]".to_string()),
        ] {
            sqlx::query(
                "INSERT INTO cloudtalk_sms_templates
                   (company_id, user_id, name, body, lead_list_id, hour_delay, minute_delay, sales_rep_ids)
                 VALUES (1, NULL, 'Drip', 'Hi', ?, 0, ?, ?)",
            )
            .bind(s.list_id)
            .bind(minutes)
            .bind(reps)
            .execute(&pool)
            .await
            .unwrap();
        }
        let (customer_id, deal_id) = insert_customer_deal(&pool, &s, "leads").await;

        schedule_templates_for_deal_list(&pool, s.list_id, 1, deal_id, customer_id, s.user_id, true)
            .await
            .unwrap();
        // Their own template and the every-rep one; not the other rep's.
        assert_eq!(pending(&pool, customer_id).await, 2);
    }

    #[test_db::test]
    async fn new_deal_of_customer_with_another_deal_gets_no_sms(pool: MySqlPool) {
        let s = setup(&pool).await;
        insert_template(&pool, s.list_id, 0, None).await;
        let (customer_id, _first) = insert_customer_deal(&pool, &s, "leads").await;
        let second = sqlx::query(
            "INSERT INTO deals (customer_id, status, list_id, position, user_id) VALUES (?, 'Not Contacted Yet', ?, 0, ?)",
        )
        .bind(customer_id)
        .bind(s.list_id)
        .bind(s.user_id)
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_id();

        schedule_templates_for_deal_list(&pool, s.list_id, 1, second, customer_id, s.user_id, true)
            .await
            .unwrap();
        assert_eq!(pending(&pool, customer_id).await, 0);
    }
}
