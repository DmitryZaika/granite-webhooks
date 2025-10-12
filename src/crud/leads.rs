use crate::schemas::add_customer::{FaceBookContactForm, NewLeadForm, WordpressContactForm};
use sqlx::mysql::MySqlQueryResult;
use sqlx::{MySqlPool, query};
use std::fmt;

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

pub async fn update_lead_from_wordpress(
    pool: &MySqlPool,
    data: &WordpressContactForm,
    company_id: i32,
    id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    return query!(
        r#"UPDATE customers
           SET name = ?, email = ?, phone = ?, postal_code = ?, address = ?, remodal_type = ?, project_size = ?, contact_time = ?, remove_and_dispose = ?, improve_offer = ?, sink = ?, backsplash = ?, kitchen_stove = ?, your_message = ?, attached_file = ?, company_id = ?
           WHERE id = ?"#,
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
        id,
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

pub async fn update_lead_from_facebook(
    pool: &MySqlPool,
    data: &FaceBookContactForm,
    company_id: i32,
    id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    return query!(
            r#"UPDATE customers
               SET phone = ?, remove_and_dispose = ?, details = ?, email = ?, city = ?, postal_code = ?, compaign_name = ?, adset_name = ?, ad_name = ?, company_id = ?
               WHERE id = ?"#,
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
            id,
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

pub async fn assign_lead(
    pool: &MySqlPool,
    lead_id: i32,
    user_id: i64,
) -> Result<MySqlQueryResult, sqlx::Error> {
    return query!(
        r#"UPDATE customers SET sales_rep = ?, assigned_date = NOW() WHERE id = ?"#,
        user_id,
        lead_id,
    )
    .execute(pool)
    .await;
}

pub async fn create_deal(
    pool: &MySqlPool,
    customer_id: i32,
    list_id: i32,
    next_pos: i32,
    sales_rep: i64,
) -> Result<MySqlQueryResult, sqlx::Error> {
    return query!(
        r#"INSERT INTO deals (customer_id, status, list_id, position, user_id) VALUES (?,?,?,?,?)"#,
        customer_id,
        "New Customer",
        list_id,
        next_pos,
        sales_rep,
    )
    .execute(pool)
    .await;
}

pub async fn create_lead_from_new_lead_form(
    pool: &MySqlPool,
    data: &NewLeadForm,
    company_id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    return query!(
            r#"INSERT INTO customers
               (name, phone, address, remove_and_dispose, details, email, city, postal_code, compaign_name, adset_name, ad_name, remodal_type, project_size, contact_time, when_start, improve_offer, sink, kitchen_stove, backsplash, your_message, attached_file, company_id, referral_source, source)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            data.name,
            data.phone,
            data.address,
            data.remove_and_dispose,
            data.details,
            data.email,
            data.city,
            data.postal_code,
            data.compaign_name,
            data.adset_name,
            data.ad_name,
            data.remodal_type,
            data.project_size,
            data.contact_time,
            data.when_start,
            data.improve_offer,
            data.sink,
            data.kitchen_stove,
            data.backsplash,
            data.your_message,
            data.attached_file,
            company_id,
            data.referral_source,
            "leads"
        )
        .execute(pool)
        .await;
}

pub async fn update_lead_from_new_lead_form(
    pool: &MySqlPool,
    data: &NewLeadForm,
    company_id: i32,
    id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    return query!(
            r#"UPDATE customers
               SET phone = ?, address = ?, remove_and_dispose = ?, details = ?, email = ?, city = ?, postal_code = ?, compaign_name = ?, adset_name = ?, ad_name = ?, remodal_type = ?, project_size = ?, contact_time = ?, when_start = ?, improve_offer = ?, sink = ?, kitchen_stove = ?, backsplash = ?, your_message = ?, attached_file = ?, company_id = ?, referral_source = ?, source = ?
               WHERE id = ?"#,
            data.phone,
            data.address,
            data.remove_and_dispose,
            data.details,
            data.email,
            data.city,
            data.postal_code,
            data.compaign_name,
            data.adset_name,
            data.ad_name,
            data.remodal_type,
            data.project_size,
            data.contact_time,
            data.when_start,
            data.improve_offer,
            data.sink,
            data.kitchen_stove,
            data.backsplash,
            data.your_message,
            data.attached_file,
            company_id,
            data.referral_source,
            "leads",
            id,
        )
        .execute(pool)
        .await;
}

pub async fn check_lead_exists(
    pool: &MySqlPool,
    email: &str,
    phone: &str,
    company_id: i32,
) -> Result<bool, sqlx::Error> {
    let row = query!(
        r#"
        SELECT COUNT(*) as count 
        FROM customers 
        WHERE company_id = ? 
          AND (email = ? OR phone = ?)
        "#,
        company_id,
        email,
        phone,
    )
    .fetch_one(pool)
    .await?;

    Ok(row.count > 0)
}

pub struct ExistingCustomer {
    pub id: i32,
    pub name: Option<String>,
    pub sales_rep: Option<i32>,
}

pub struct Deal {
    pub id: u64,
    pub user_id: Option<i32>,
}

pub async fn find_existing_customer(
    pool: &MySqlPool,
    email: &Option<&str>,
    phone: &Option<&str>,
    company_id: i32,
) -> Result<Option<ExistingCustomer>, sqlx::Error> {
    if email.is_none() && phone.is_none() {
        return Ok(None);
    }
    sqlx::query_as!(
        ExistingCustomer,
        r#"SELECT id, name, sales_rep FROM customers WHERE company_id = ? AND (email = ? OR phone = ?) ORDER BY id DESC LIMIT 1"#,
        company_id,
        email,
        phone
    )
    .fetch_optional(pool)
    .await
}

pub async fn get_existing_deal(
    pool: &MySqlPool,
    customer_id: i32,
) -> Result<Option<Deal>, sqlx::Error> {
    sqlx::query_as!(
        Deal,
        r#"SELECT id, user_id FROM deals WHERE customer_id = ? AND list_id != 4 AND deleted_at IS NULL ORDER BY id DESC LIMIT 1"#,
        customer_id
    )
    .fetch_optional(pool)
    .await
}

pub async fn create_deal_from_lead(
    pool: &MySqlPool,
    lead_id: i32,
    user_id: i64,
) -> Result<MySqlQueryResult, sqlx::Error> {
    return query!(
        r#"INSERT INTO deals (customer_id, status, list_id, user_id) VALUES (?,?,?,?)"#,
        lead_id,
        "New Customer",
        1,
        user_id,
    )
    .execute(pool)
    .await;
}

pub async fn deal_check(pool: &MySqlPool, customer_id: i32) -> Result<bool, sqlx::Error> {
    let row = query!(
        r#"SELECT id FROM deals WHERE customer_id = ? AND list_id != 4 AND deleted_at IS NULL"#,
        customer_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some_and(|r| r.id > 0))
}

pub enum LeadForm {
    NewLeadForm(NewLeadForm),
    WordpressContactForm(WordpressContactForm),
    FaceBookContactForm(FaceBookContactForm),
}

impl LeadForm {
    pub async fn update_lead(
        &self,
        pool: &MySqlPool,
        company_id: i32,
        id: i32,
    ) -> Result<MySqlQueryResult, sqlx::Error> {
        match self {
            Self::NewLeadForm(data) => {
                update_lead_from_new_lead_form(pool, data, company_id, id).await
            }
            Self::WordpressContactForm(data) => {
                update_lead_from_wordpress(pool, data, company_id, id).await
            }
            Self::FaceBookContactForm(data) => {
                update_lead_from_facebook(pool, data, company_id, id).await
            }
        }
    }
}

impl fmt::Display for LeadForm {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::NewLeadForm(data) => writeln!(f, "{data}"),
            Self::WordpressContactForm(data) => writeln!(f, "{data}"),
            Self::FaceBookContactForm(data) => writeln!(f, "{data}"),
        }
    }
}
