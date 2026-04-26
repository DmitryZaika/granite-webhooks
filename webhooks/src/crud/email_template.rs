use sqlx::MySqlPool;

pub struct EmailTemplate {
    id: i32,
    hour_delay: Option<i32>,
}

pub async fn get_templates_from_list_id(
    pool: &MySqlPool,
    list_id: i32,
) -> Result<Option<EmailTemplate>, sqlx::Error> {
    if list_id == 1 {
        // Don't send templates on the default group
        return Ok(None);
    }
    sqlx::query_as!(
        EmailTemplate,
        r#"
        SELECT email_templates.id, email_templates.hour_delay
        FROM email_templates
        JOIN groups_list ON email_templates.lead_group_id = groups_list.id
        JOIN deals_list ON deals_list.group_id = groups_list.id
        WHERE deals_list.id = ?
          AND groups_list.id != 1
        LIMIT 1
        "#,
        list_id
    )
    .fetch_optional(pool)
    .await
}
