use crate::cloudtalk::schemas::{ContactPayload, ContactSearchEnvelope, CountriesEnvelope};
use crate::cloudtalk::utils::{
    build_payload, coerce_id, extract_id, extract_phones, find_contact_id, is_united_states,
    upsert_contact,
};
use crate::crud::cloudtalk::{company_has_cloud_talk, get_auth_string, load_customer_with_mapping};
use crate::libs::constants::{NOT_FOUND_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use axum::http::StatusCode;
use lambda_http::tracing;
use reqwest::{Client, Method};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::MySqlPool;

const BASE_URL: &str = "https://my.cloudtalk.io/api";

pub async fn cloudtalk_request<T: Serialize + Sync, R: DeserializeOwned + Send>(
    pool: &MySqlPool,
    client: &Client,
    path: &str,
    company_id: u64,
    method: Method,
    body: Option<&T>,
) -> Result<R, Box<dyn std::error::Error + Send + Sync>> {
    let auth = get_auth_string(pool, company_id).await?;
    let url = format!("{BASE_URL}/{path}");

    let mut req = client
        .request(method.clone(), &url)
        .header("Authorization", format!("Basic {auth}"))
        .header("Accept", "application/json");

    if let Some(b) = body {
        // .json() automatically sets Content-Type to application/json
        req = req.json(b);
    }

    let response = match req.send().await {
        Ok(res) => res,
        Err(e) => return Err(Box::new(e)),
    };

    // If the API returns an HTTP error code (4xx/5xx), you might want to handle it here
    // before attempting to deserialize. For example:
    // let response = response.error_for_status()?;

    // Deserialize the response body directly into the generic type R
    let parsed_response = response.json::<R>().await?;

    Ok(parsed_response)
}

pub async fn get_cloudtalk_us_country_id(
    pool: &MySqlPool,
    client: &Client,
    company_id: u64,
) -> Option<u64> {
    // The try/catch block is achieved here by safely transforming errors to None using `.ok()?`
    let response: CountriesEnvelope = cloudtalk_request::<(), CountriesEnvelope>(
        pool,
        client,
        "countries/index.json",
        company_id,
        Method::GET,
        None,
    )
    .await
    .ok()?;

    let items = response.response_data?.data?;

    for item in items {
        let country = item.into_country();

        if is_united_states(&country)
            && let Some(id_val) = &country.id
            && let Some(id) = coerce_id(id_val)
        {
            return Some(id); // Found it, break and return early
        }
    }

    None
}

pub async fn update_cloudtalk_contact(
    pool: &MySqlPool,
    client: &Client,
    company_id: u64,
    cloudtalk_id: i64,
    payload: &ContactPayload,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = format!("contacts/edit/{cloudtalk_id}.json");

    // We pass `serde_json::Value` as the response type (R) to consume
    // whatever JSON CloudTalk returns without needing to map it.
    let _response: serde_json::Value =
        cloudtalk_request(pool, client, &path, company_id, Method::POST, Some(payload)).await?;

    Ok(())
}

pub async fn sync_customer_to_cloud_talk(
    pool: &MySqlPool,
    client: &Client,
    customer_id: i32,
) -> BasicResponse {
    let mapping = match load_customer_with_mapping(pool, customer_id).await {
        Ok(Some(mapping)) => mapping,
        Ok(None) => return NOT_FOUND_RESPONSE,
        Err(error) => {
            tracing::error!(?error, customer_id, "Failed to load customer with mapping");
            return internal_error("Failed to load customer with mapping");
        }
    };
    let Some(company_id) = mapping.company_id else {
        return internal_error("The given user does not have a company");
    };
    let clean_company_id: u64 = match company_id.try_into() {
        Ok(clean_company_id) => clean_company_id,
        Err(error) => {
            tracing::error!(?error, company_id, "Failed to convert company ID to u64");
            return internal_error("Failed to convert company ID to u64");
        }
    };
    match company_has_cloud_talk(pool, company_id).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::error!(
                company_id,
                customer_id,
                "Cloudtalk not configured for this company"
            );
            return (
                StatusCode::UNAUTHORIZED,
                "Cloudtalk not configured for this company",
            );
        }
        Err(error) => {
            tracing::error!(
                ?error,
                company_id,
                customer_id,
                "Failed to check cloudtalk configuration"
            );
            return internal_error("Failed to check cloudtalk configuration");
        }
    }
    let us_country_id = get_cloudtalk_us_country_id(pool, client, clean_company_id).await;
    let payload = build_payload(&mapping, us_country_id);
    let Some(clean_payload) = payload.await else {
        return internal_error("Failed to build payload");
    };
    upsert_contact(pool, client, &mapping, &clean_payload, clean_company_id).await
}

pub async fn create_cloudtalk_contact(
    pool: &MySqlPool,
    client: &Client,
    company_id: u64,
    payload: &ContactPayload,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let json: serde_json::Value = cloudtalk_request(
        pool,
        client,
        "contacts/add.json",
        company_id,
        Method::PUT,
        Some(payload),
    )
    .await?;

    if let Some(id_from_response) = find_contact_id(&json) {
        return Ok(id_from_response);
    }

    // Response shape unfamiliar; log the structural mismatch and fail immediately.
    let preview = json.to_string();
    let truncated_preview: String = preview.chars().take(500).collect();

    eprintln!(
        "[cloudtalk-unparseable-response] {:?}",
        serde_json::json!({
            "endpoint": "PUT contacts/add.json",
            "companyId": company_id,
            "preview": truncated_preview,
        })
    );

    Err("CloudTalk add.json: unable to resolve contact id".into())
}

pub async fn find_contact_by_one_phone(
    pool: &MySqlPool,
    client: &Client,
    company_id: u64,
    e164_phone: &str,
) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
    // URL-encode the phone number parameter safely
    let encoded_phone = urlencoding::encode(e164_phone);
    let path = format!("contacts/index.json?keyword={encoded_phone}&limit=10");

    // Make the request using our generic helper
    let json: ContactSearchEnvelope = cloudtalk_request(
        pool,
        client,
        &path,
        company_id,
        Method::GET,
        None::<&()>, // No body sent for GET request
    )
    .await?;

    // Safely extract the data array, defaulting to an empty vector if it's null/missing
    let hits = json
        .response_data
        .and_then(|rd| rd.data)
        .unwrap_or_default();

    for hit in hits {
        // Check if the extracted numbers contain the target e164_phone
        if extract_phones(&hit).iter().any(|phone| phone == e164_phone)
            && let Some(id) = extract_id(&hit)
        {
            return Ok(Some(id)); // Return early with the found ID
        }
    }

    Ok(None) // Return None if no matching contact was found
}

pub async fn find_cloudtalk_contact_by_phone(
    pool: &MySqlPool,
    client: &Client,
    company_id: u64,
    e164_phones: &[String],
) -> Result<Option<i64>, Box<dyn std::error::Error + Send + Sync>> {
    for phone in e164_phones {
        if let Some(id) = find_contact_by_one_phone(pool, client, company_id, phone).await? {
            return Ok(Some(i64::try_from(id)?));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use axum::http::StatusCode;
    use sqlx::MySqlPool;

    // Adjust this instantiation based on your actual `Client` type
    // (e.g., reqwest::Client or a custom application HTTP client wrapper)
    fn setup_mock_client() -> Client {
        Client::default()
    }

    async fn create_company(
        pool: &MySqlPool,
        name: &str,
        has_credentials: bool,
    ) -> Result<u64, sqlx::Error> {
        let key = if has_credentials {
            Some("mock_access_key")
        } else {
            None
        };
        let secret = if has_credentials {
            Some("mock_access_secret")
        } else {
            None
        };

        let rec = sqlx::query!(
            r#"
            INSERT INTO company (name, cloudtalk_access_key, cloudtalk_access_secret)
            VALUES (?, ?, ?)
            "#,
            name,
            key,
            secret
        )
        .execute(pool)
        .await?;

        Ok(rec.last_insert_id())
    }

    async fn create_customer(
        pool: &MySqlPool,
        company_id: Option<i32>,
        phone: Option<&str>,
        phone_2: Option<&str>,
        address: Option<&str>,
    ) -> Result<u64, sqlx::Error> {
        let rec = sqlx::query!(
            r#"
            INSERT INTO customers (company_id, name, phone, phone_2, email, address)
            VALUES (?, 'Acme Co', ?, ?, 'a@b.com', ?)
            "#,
            company_id,
            phone,
            phone_2,
            address
        )
        .execute(pool)
        .await?;

        Ok(rec.last_insert_id())
    }

    async fn insert_cloudtalk_contact_mapping(
        pool: &MySqlPool,
        customer_id: i32,
        company_id: i32,
        cloudtalk_id: i32,
    ) -> Result<u64, sqlx::Error> {
        let rec = sqlx::query!(
            r#"
            INSERT INTO cloudtalk_contacts (customer_id, company_id, cloudtalk_id)
            VALUES (?, ?, ?)
            "#,
            customer_id,
            company_id,
            cloudtalk_id
        )
        .execute(pool)
        .await?;

        Ok(rec.last_insert_id())
    }

    // -----------------------------------------------------------------
    // sync_customer_to_cloud_talk Tests
    // -----------------------------------------------------------------

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sync_customer_not_found(pool: MySqlPool) {
        let client = setup_mock_client();

        // Target a completely non-existent customer ID
        let res = sync_customer_to_cloud_talk(&pool, &client, 9999).await;

        // Should return your designated NOT_FOUND_RESPONSE status code
        assert_eq!(res.0, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sync_customer_missing_company(pool: MySqlPool) {
        let client = setup_mock_client();

        // Create a customer without a company_id association
        let customer_id = create_customer(&pool, None, Some("317-316-1456"), None, None)
            .await
            .unwrap();

        let res = sync_customer_to_cloud_talk(&pool, &client, customer_id as i32).await;

        assert_eq!(res.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(res.1.contains("The given user does not have a company"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sync_company_missing_cloud_talk_credentials(pool: MySqlPool) {
        let client = setup_mock_client();

        // Seed a company explicitly setting credentials to NULL
        let company_id = create_company(&pool, "No CloudTalk Co", false)
            .await
            .unwrap();
        let customer_id = create_customer(
            &pool,
            Some(company_id as i32),
            Some("317-316-1456"),
            None,
            None,
        )
        .await
        .unwrap();

        let res = sync_customer_to_cloud_talk(&pool, &client, customer_id as i32).await;

        assert_eq!(res.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(res.1.contains("Cloudtalk not configured for this company"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sync_customer_no_usable_phone_fails_payload_build(pool: MySqlPool) {
        let client = setup_mock_client();

        let company_id = create_company(&pool, "Valid Credentials Co", true)
            .await
            .unwrap();
        // Passing None/Invalid properties to hit the `payload == None` guard
        let customer_id = create_customer(
            &pool,
            Some(company_id as i32),
            None,
            Some("invalid_phone"),
            None,
        )
        .await
        .unwrap();

        let res = sync_customer_to_cloud_talk(&pool, &client, customer_id as i32).await;

        assert_eq!(res.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(res.1.contains("Failed to build payload"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sync_customer_success_path(pool: MySqlPool) {
        let client = setup_mock_client();

        let company_id = create_company(&pool, "Fully Configured Co", true)
            .await
            .unwrap();
        let customer_id = create_customer(
            &pool,
            Some(company_id as i32),
            Some("317-316-1456"),
            None,
            Some("3333 N Tacoma Ave, Indianapolis, IN 46218, USA"),
        )
        .await
        .unwrap();

        // Execution path will call get_cloudtalk_us_country_id, build_payload, and upsert_contact
        let res = sync_customer_to_cloud_talk(&pool, &client, customer_id as i32).await;

        assert_eq!(res.0, StatusCode::OK);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sync_customer_with_existing_mapping_reuses_id(pool: MySqlPool) {
        let client = setup_mock_client();

        let company_id = create_company(&pool, "Fully Configured Co", true)
            .await
            .unwrap();
        let customer_id = create_customer(
            &pool,
            Some(company_id as i32),
            Some("317-316-1456"),
            None,
            None,
        )
        .await
        .unwrap();

        // Setup an existing mapping reference in the DB table beforehand
        insert_cloudtalk_contact_mapping(&pool, customer_id as i32, company_id as i32, 4242)
            .await
            .unwrap();

        let res = sync_customer_to_cloud_talk(&pool, &client, customer_id as i32).await;

        assert_eq!(res.0, StatusCode::OK);

        // Confirm mapping configuration remains accurately updated/untouched
        let mapping_exists = sqlx::query!(
            "SELECT cloudtalk_id FROM cloudtalk_contacts WHERE customer_id = ?",
            customer_id as i32
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(mapping_exists.cloudtalk_id, 4242);
    }
}
