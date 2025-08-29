use crate::schemas::add_customer::{FaceBookContactForm, WordpressContactForm};
use sqlx::mysql::MySqlQueryResult;
use sqlx::{query, MySqlPool};

pub async fn create_lead_from_wordpress(
    pool: &MySqlPool,
    data: &WordpressContactForm,
    company_id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    return query!(
        r#"INSERT INTO customers
           (name, email, phone, postal_code, address, remodal_type, project_size, contact_time, remove_and_dispose, improve_offer, sink, backsplash, kitchen_stove, your_message, attached_file, company_id, referral_source, source)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        data.name,
        data.email,
        data.phone,
        data.postal_code,
        data.address,
        data.remodal_type,
        data.project_size,
        data.contact_time,
        data.remove_and_dispose,
        data.improve_offer,
        data.sink,
        data.backsplash,
        data.kitchen_stove,
        data.your_message,
        data.attached_file,
        company_id,
        "wordpress-form",
        "leads"
    )
    .execute(pool).await;
}

pub async fn create_lead_from_facebook(
    pool: &MySqlPool,
    data: &FaceBookContactForm,
    company_id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    return query!(
            r#"INSERT INTO customers
               (name, phone, remove_and_dispose, details, email, city, postal_code, compaign_name, adset_name, ad_name, company_id, referral_source, source)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            data.name,
            data.phone,
            data.remove_and_dispose,
            data.details,
            data.email,
            data.city,
            data.postal_code,
            data.compaign_name,
            data.adset_name,
            data.ad_name,
            company_id,
            "facebook-form",
            "leads"
        )
        .execute(pool)
        .await;
}

pub async fn update_lead_asignee(
    pool: &MySqlPool,
    sales_rep: i32,
    id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    return query!(
            r#"UPDATE customers
               SET sales_rep = ?
               WHERE id = ?"#,
            sales_rep,
            id,
        )
        .execute(pool)
        .await;
}
