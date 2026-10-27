use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;

pub struct EmailTemplate {
    pub id: i32,
    pub hour_delay: Option<i32>,
}

/// The company email drips of `list_id` that go out for deals of `user_id`:
/// those naming no sales reps, or naming them.
pub async fn get_templates_for_list_id(
    pool: &MySqlPool,
    list_id: i32,
    company_id: i32,
    user_id: i32,
) -> Result<Vec<EmailTemplate>, sqlx::Error> {
    let rows: Vec<(i32, Option<i32>)> = sqlx::query_as(
        r#"
        SELECT email_templates.id, email_templates.hour_delay
        FROM email_templates
        WHERE email_templates.lead_list_id = ?
          AND email_templates.company_id = ?
          AND email_templates.user_id IS NULL
          AND email_templates.deleted_at IS NULL
          AND (email_templates.sales_rep_ids IS NULL
               OR JSON_LENGTH(email_templates.sales_rep_ids) = 0
               OR JSON_CONTAINS(email_templates.sales_rep_ids, CAST(? AS JSON)))
        ORDER BY email_templates.hour_delay ASC, email_templates.id ASC
        "#,
    )
    .bind(list_id)
    .bind(company_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, hour_delay)| EmailTemplate { id, hour_delay })
        .collect())
}

pub async fn get_template_from_list_id(
    pool: &MySqlPool,
    list_id: i32,
    company_id: i32,
    user_id: i32,
) -> Result<Option<EmailTemplate>, sqlx::Error> {
    let templates = get_templates_for_list_id(pool, list_id, company_id, user_id).await?;
    Ok(templates.into_iter().next())
}

pub struct CreateEmailTemplate {
    pub template_name: String,
    pub template_subject: String,
    pub template_body: String,
    pub company_id: i32,
    pub lead_group_id: Option<i32>,
    pub lead_list_id: Option<i32>,
    pub hour_delay: Option<i32>,
    pub show_template: bool, // Maps to tinyint(1)
}

pub async fn insert_email_template(
    pool: &MySqlPool,
    template: CreateEmailTemplate,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO email_templates (
            template_name,
            template_subject,
            template_body,
            company_id,
            lead_group_id,
            lead_list_id,
            hour_delay,
            show_template
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        template.template_name,
        template.template_subject,
        template.template_body,
        template.company_id,
        template.lead_group_id,
        template.lead_list_id,
        template.hour_delay,
        template.show_template
    )
    .execute(pool)
    .await
}
