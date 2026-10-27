use sqlx::MySqlPool;

use crate::crud::user::get_user_template;

#[derive(serde::Serialize, Default)]
pub struct UserVariableData {
    pub name: Option<String>,
    pub email: Option<String>,
    pub email_name: Option<String>,
    pub phone_number: Option<String>,
}

#[derive(serde::Serialize, Default)]
pub struct InfoVariableData {
    pub id: Option<i32>,
    pub name: Option<String>,
    pub address: Option<String>,
    pub hours_of_operation: Option<String>,
    pub domain: Option<String>,
    pub subdomain: Option<String>,
    /// Customer only: the customer's primary email address.
    pub email: Option<String>,
    /// Customer only: the customer's main phone number.
    pub phone: Option<String>,
}

/// The deal a message is sent from (`{{deal.*}}`).
#[derive(serde::Serialize, Default, Clone)]
pub struct DealVariableData {
    pub name: Option<String>,
    /// Dollars.
    pub amount: Option<f64>,
}

/// The quote being emailed (`{{quote.*}}`). Only the quote email fills it.
#[derive(serde::Serialize, Default, Clone)]
pub struct QuoteVariableData {
    pub name: Option<String>,
    /// Dollars: the accepted option's total, else the first option's.
    pub total: Option<f64>,
    /// Material and color of that option, e.g. "Quartz Calacatta Laza".
    pub color: Option<String>,
    /// The customer's link to the quote.
    pub link: Option<String>,
    /// `YYYY-MM-DD`.
    pub expiration: Option<String>,
}

#[derive(serde::Serialize, Default)]
pub struct TemplateVariableData {
    pub user: UserVariableData,
    pub customer: Option<InfoVariableData>,
    pub company: Option<InfoVariableData>,
    pub deal: Option<DealVariableData>,
    pub quote: Option<QuoteVariableData>,
}

/// Like `fetch_template_variable_data`, but uses an explicit `company_id`
/// for the customer lookup instead of deriving it from the user record.
/// This is needed for scheduled emails where the user's current company
/// may differ from the company that owns the deal/customer.
pub async fn fetch_template_variable_data(
    pool: &MySqlPool,
    user_id: i32,
    deal_id: Option<i32>,
    customer_id: Option<i32>,
    company_id: i32,
) -> Result<TemplateVariableData, sqlx::Error> {
    let user = get_user_template(pool, user_id).await?;
    let (customer_data, company_data, deal_data) = tokio::try_join!(
        fetch_customer_data(pool, deal_id, customer_id, company_id),
        fetch_company_data(pool, Some(company_id)),
        fetch_deal_data(pool, deal_id, company_id)
    )?;

    let clean_user = UserVariableData {
        name: user.name,
        email: user.email,
        email_name: user.email_name,
        phone_number: user.phone_number,
    };

    Ok(TemplateVariableData {
        user: clean_user,
        customer: customer_data,
        company: company_data,
        deal: deal_data,
        quote: None,
    })
}

#[derive(sqlx::FromRow)]
struct CustomerRow {
    name: Option<String>,
    address: Option<String>,
    phone: Option<String>,
    email: Option<String>,
}

impl From<CustomerRow> for InfoVariableData {
    fn from(r: CustomerRow) -> Self {
        Self {
            name: r.name,
            address: r.address,
            phone: r.phone,
            email: r.email,
            ..Default::default()
        }
    }
}

async fn fetch_customer_data(
    pool: &MySqlPool,
    deal_id: Option<i32>,
    customer_id: Option<i32>,
    company_id: i32,
) -> Result<Option<InfoVariableData>, sqlx::Error> {
    if deal_id.is_none() && customer_id.is_none() {
        return Ok(None);
    }

    if let Some(d_id) = deal_id {
        let row = sqlx::query_as::<_, CustomerRow>(
            "SELECT c.name, c.address, c.phone, ce.email
             FROM deals d
             JOIN customers c ON d.customer_id = c.id
             LEFT JOIN customers_emails ce ON ce.id = c.email_id
             WHERE d.id = ? AND d.deleted_at IS NULL AND c.company_id = ?
             LIMIT 1",
        )
        .bind(d_id)
        .bind(company_id)
        .fetch_optional(pool)
        .await?;

        if let Some(r) = row {
            return Ok(Some(r.into()));
        }
    }

    if let Some(c_id) = customer_id {
        let row = sqlx::query_as::<_, CustomerRow>(
            "SELECT c.name, c.address, c.phone, ce.email
             FROM customers c
             LEFT JOIN customers_emails ce ON ce.id = c.email_id
             WHERE c.id = ? AND c.deleted_at IS NULL AND c.company_id = ?
             LIMIT 1",
        )
        .bind(c_id)
        .bind(company_id)
        .fetch_optional(pool)
        .await?;

        if let Some(r) = row {
            return Ok(Some(r.into()));
        }
    }

    Ok(None)
}

async fn fetch_deal_data(
    pool: &MySqlPool,
    deal_id: Option<i32>,
    company_id: i32,
) -> Result<Option<DealVariableData>, sqlx::Error> {
    let Some(d_id) = deal_id else {
        return Ok(None);
    };
    let row: Option<(Option<String>, Option<f64>)> = sqlx::query_as(
        "SELECT d.title, CAST(d.amount AS DOUBLE)
         FROM deals d
         JOIN customers c ON d.customer_id = c.id
         WHERE d.id = ? AND d.deleted_at IS NULL AND c.company_id = ?
         LIMIT 1",
    )
    .bind(d_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(name, amount)| DealVariableData { name, amount }))
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
        SELECT id, name, address, hours_of_operation, domain, subdomain
        FROM company
        WHERE id = ?
        LIMIT 1
        "#,
        c_id
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| InfoVariableData {
        id: Some(r.id),
        name: Some(r.name),
        address: r.address,
        hours_of_operation: r.hours_of_operation,
        domain: r.domain,
        subdomain: r.subdomain,
        ..Default::default()
    }))
}
