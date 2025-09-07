use sqlx::MySqlPool;

struct Company {
    address: Option<String>,
}

pub async fn get_company_address(pool: &MySqlPool, company_id: i32) -> Result<Option<String>, sqlx::Error> {
    let company = sqlx::query_as!(
        Company,
        r#"SELECT address FROM company WHERE id = ?"#,
        company_id
    )
    .fetch_optional(pool)
    .await?;
    if let Some(company) = company {
        return Ok(company.address);
    }
    return Ok(None);
   
}