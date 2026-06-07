use sqlx::MySqlPool;

use crate::crud::user::get_user_template;

#[derive(serde::Serialize)]
pub struct UserVariableData {
    pub name: Option<String>,
    pub email: Option<String>,
    pub phone_number: Option<String>,
}

#[derive(serde::Serialize)]
pub struct InfoVariableData {
    pub name: Option<String>,
    pub address: Option<String>,
}

#[derive(serde::Serialize)]
pub struct TemplateVariableData {
    pub user: UserVariableData,
    pub customer: Option<InfoVariableData>,
    pub company: Option<InfoVariableData>,
}

pub async fn fetch_template_variable_data(
    pool: &MySqlPool,
    user_id: i32,
    deal_id: Option<i32>,
    customer_id: Option<i32>,
) -> Result<TemplateVariableData, sqlx::Error> {
    let user = get_user_template(pool, user_id).await?;
    let (customer_data, company_data) = tokio::try_join!(
        fetch_customer_data(pool, deal_id, customer_id, user.company_id),
        fetch_company_data(pool, user.company_id)
    )?;

    let clean_user = UserVariableData {
        name: user.name,
        email: user.email,
        phone_number: user.phone_number,
    };

    Ok(TemplateVariableData {
        user: clean_user,
        customer: customer_data,
        company: company_data,
    })
}

async fn fetch_customer_data(
    pool: &MySqlPool,
    deal_id: Option<i32>,
    customer_id: Option<i32>,
    company_id: Option<i32>,
) -> Result<Option<InfoVariableData>, sqlx::Error> {
    if deal_id.is_none() && customer_id.is_none() {
        return Ok(None);
    }

    if let Some(d_id) = deal_id {
        let row = sqlx::query!(
            r#"
            SELECT c.name, c.address
            FROM deals d
            JOIN customers c ON d.customer_id = c.id
            WHERE d.id = ? AND d.deleted_at IS NULL AND c.company_id = ?
            LIMIT 1
            "#,
            d_id,
            company_id
        )
        .fetch_optional(pool)
        .await?;

        if let Some(r) = row {
            return Ok(Some(InfoVariableData {
                name: r.name,
                address: r.address,
            }));
        }
    }

    if let Some(c_id) = customer_id {
        let row = sqlx::query!(
            r#"
            SELECT name, address
            FROM customers
            WHERE id = ? AND deleted_at IS NULL AND company_id = ?
            LIMIT 1
            "#,
            c_id,
            company_id
        )
        .fetch_optional(pool)
        .await?;

        if let Some(r) = row {
            return Ok(Some(InfoVariableData {
                name: r.name,
                address: r.address,
            }));
        }
    }

    Ok(None)
}

async fn fetch_company_data(
    pool: &MySqlPool,
    company_id: Option<i32>,
) -> Result<Option<InfoVariableData>, sqlx::Error> {
    let c_id = match company_id {
        Some(id) => id,
        None => return Ok(None),
    };

    let row = sqlx::query!(
        r#"
        SELECT name, address
        FROM company
        WHERE id = ?
        LIMIT 1
        "#,
        c_id
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| InfoVariableData {
        name: Some(r.name),
        address: r.address,
    }))
}
