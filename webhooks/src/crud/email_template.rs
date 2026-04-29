use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;

pub struct EmailTemplate {
    pub id: i32,
    pub hour_delay: Option<i32>,
}

pub async fn get_template_from_list_id(
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

pub struct CreateEmailTemplate {
    pub template_name: String,
    pub template_subject: String,
    pub template_body: String,
    pub company_id: i32,
    pub lead_group_id: Option<i32>,
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
            hour_delay,
            show_template
        )
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
        template.template_name,
        template.template_subject,
        template.template_body,
        template.company_id,
        template.lead_group_id,
        template.hour_delay,
        template.show_template
    )
    .execute(pool)
    .await
}
