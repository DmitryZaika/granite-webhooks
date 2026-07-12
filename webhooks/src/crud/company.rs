use sqlx::MySqlPool;

struct Company {
    address: Option<String>,
    latitude: Option<f64>,
    longitude: Option<f64>,
}

pub async fn get_company_address(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Option<String>, sqlx::Error> {
    let company = sqlx::query_as!(
        Company,
        "SELECT address, latitude, longitude FROM company WHERE id = ?",
        &company_id
    )
    .fetch_optional(pool)
    .await?;
    if let Some(company) = company {
        return Ok(company.address);
    }
    Ok(None)
}

pub struct CompanyCoordinates {
    pub latitude: f64,
    pub longitude: f64,
}

pub async fn get_company_coordinates(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Option<CompanyCoordinates>, sqlx::Error> {
    let company = sqlx::query_as!(
        Company,
        "SELECT address, latitude, longitude FROM company WHERE id = ?",
        &company_id
    )
    .fetch_optional(pool)
    .await?;
    if let Some(company) = company {
        match (company.latitude, company.longitude) {
            (Some(lat), Some(lng)) => {
                return Ok(Some(CompanyCoordinates {
                    latitude: lat,
                    longitude: lng,
                }));
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

pub async fn update_company_coordinates(
    pool: &MySqlPool,
    company_id: i32,
    latitude: f64,
    longitude: f64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE company SET latitude = ?, longitude = ? WHERE id = ?",
        latitude,
        longitude,
        &company_id
    )
    .execute(pool)
    .await?;
    Ok(())
}
