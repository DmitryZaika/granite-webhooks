use crate::schemas::add_customer::{FaceBookContactForm, NewLeadForm, WordpressContactForm};
use sqlx::mysql::MySqlQueryResult;
use sqlx::{MySqlPool, query};

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
    email: Option<&str>,
    phone: Option<&str>,
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

pub async fn get_default_list_id_from_company_id(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<i32, sqlx::Error> {
    let id = sqlx::query_scalar!(
        r#"
        SELECT dl.id 
        FROM deals_list dl
        INNER JOIN groups_list gl ON dl.group_id = gl.id
        WHERE gl.company_id = ?
          AND gl.is_default = 1
          AND dl.deleted_at IS NULL
          AND gl.deleted_at IS NULL
        ORDER BY dl.position ASC, dl.id ASC 
        LIMIT 1
        "#,
        company_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(id.unwrap_or(1))
}

pub async fn create_deal_from_lead(
    pool: &MySqlPool,
    lead_id: i32,
    user_id: i64,
    default_list_id: i32,
    position: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    return query!(
        r#"INSERT INTO deals (customer_id, status, list_id, user_id, position) VALUES (?,?,?,?,?)"#,
        lead_id,
        "New Customer",
        default_list_id,
        user_id,
        position,
    )
    .execute(pool)
    .await;
}


mod tests {
    use super::*;

    async fn insert_company(pool: &MySqlPool, company_id: i32) -> Result<i32, sqlx::Error> {
        let rec = sqlx::query!(
            r#"INSERT INTO company (name) VALUES ('Test Company')"#,
        )
        .execute(pool)
        .await?;
        Ok(rec.last_insert_id() as i32)
    }

    async fn insert_group_list(pool: &MySqlPool, company_id: i32) -> Result<u64, sqlx::Error> {

        let rec = sqlx::query!(
            r#"INSERT INTO groups_list (name, company_id, is_default) VALUES ('Test Group', ?, 1)"#,
            company_id
        )
        .execute(pool)
        .await?;
        Ok(rec.last_insert_id())
    }

    async fn insert_deals_list(pool: &MySqlPool, group_id: u64) -> Result<u64, sqlx::Error> {

        let rec = sqlx::query!(
            r#"INSERT INTO deals_list (name, group_id) VALUES ('Test Deals List', ?)"#,
            group_id,
        )
        .execute(pool)
        .await?;
        Ok(rec.last_insert_id())
    }

    #[sqlx::test]
    async fn test_get_default_list_id_from_company_id(pool: MySqlPool) {
        let id = get_default_list_id_from_company_id(&pool, 1).await.unwrap();
        assert_eq!(id, 1);
    }

    #[sqlx::test]
    async fn get_assigned_list_id_from_company_id(pool: MySqlPool) {
        let company_id = insert_company(&pool, 1).await.unwrap();
        let group_id = insert_group_list(&pool, company_id).await.unwrap();
        let list_id = insert_deals_list(&pool, group_id).await.unwrap();
        let id = get_default_list_id_from_company_id(&pool, company_id).await.unwrap();
        assert_eq!(id as u64, list_id);
    }

    #[sqlx::test]
    async fn test_multiple_default_groups(pool: MySqlPool) {
        let company_id = insert_company(&pool, 1).await.unwrap();
        
        let group_id1 = insert_group_list(&pool, company_id).await.unwrap();
        let list_id1 = insert_deals_list(&pool, group_id1).await.unwrap();
        
        let group_id2 = insert_group_list(&pool, company_id).await.unwrap();
        let _list_id2 = insert_deals_list(&pool, group_id2).await.unwrap();
        
        let id = get_default_list_id_from_company_id(&pool, company_id).await.unwrap();
        
        // Should pick the first one based on ordering (dl.id ASC)
        assert_eq!(id as u64, list_id1);
    }

    #[sqlx::test]
    async fn test_no_default_groups(pool: MySqlPool) {
        let company_id = insert_company(&pool, 1).await.unwrap();
        
        // Group that is NOT default
        sqlx::query!(
            r#"INSERT INTO groups_list (name, company_id, is_default) VALUES ('Non-default Group', ?, 0)"#,
            company_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let id = get_default_list_id_from_company_id(&pool, company_id).await.unwrap();
        
        // Should return fallback value 1
        assert_eq!(id, 1);
    }

    #[sqlx::test]
    async fn test_deleted_records_ignored(pool: MySqlPool) {
        let company_id = insert_company(&pool, 1).await.unwrap();
        let group_id = insert_group_list(&pool, company_id).await.unwrap();
        
        // Insert a deleted deals_list entry
        sqlx::query!(
            r#"INSERT INTO deals_list (name, group_id, deleted_at) VALUES ('Deleted List', ?, NOW())"#,
            group_id
        )
        .execute(&pool)
        .await
        .unwrap();

        // Insert a non-deleted one
        let valid_list_id = insert_deals_list(&pool, group_id).await.unwrap();

        let id = get_default_list_id_from_company_id(&pool, company_id).await.unwrap();
        assert_eq!(id as u64, valid_list_id);
    }

    #[sqlx::test]
    async fn test_deals_list_ordering(pool: MySqlPool) {
        let company_id = insert_company(&pool, 1).await.unwrap();
        let group_id = insert_group_list(&pool, company_id).await.unwrap();
        
        // List with position 2
        sqlx::query!(
            r#"INSERT INTO deals_list (name, group_id, position) VALUES ('Pos 2', ?, 2)"#,
            group_id
        )
        .execute(&pool)
        .await
        .unwrap();

        // List with position 1
        let pos1_id = sqlx::query!(
            r#"INSERT INTO deals_list (name, group_id, position) VALUES ('Pos 1', ?, 1)"#,
            group_id
        )
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_id();

        let id = get_default_list_id_from_company_id(&pool, company_id).await.unwrap();
        
        // Should pick the one with position 1
        assert_eq!(id as u64, pos1_id);
    }

}