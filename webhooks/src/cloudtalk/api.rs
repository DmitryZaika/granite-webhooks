use crate::cloudtalk::schemas::{ContactPayload, ContactSearchEnvelope, CountriesEnvelope};
use crate::cloudtalk::utils::{
    build_payload, coerce_id, extract_id, extract_phones, find_contact_id, is_united_states,
    upsert_contact,
};
use crate::crud::cloudtalk::{company_has_cloud_talk, get_auth_string, load_customer_with_mapping};
use crate::libs::constants::{NOT_FOUND_RESPONSE, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use reqwest::{Client, Method};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::MySqlPool;

const BASE_URL: &str = "https://my.cloudtalk.io/api";

pub async fn cloudtalk_request<T: Serialize, R: DeserializeOwned>(
    pool: &MySqlPool,
    client: &Client,
    path: &str,
    company_id: u64,
    method: Method,
    body: Option<&T>,
) -> Result<R, Box<dyn std::error::Error + Send + Sync>> {
    let auth = get_auth_string(pool, company_id).await?;
    let url = format!("{}/{}", BASE_URL, path);

    let mut req = client
        .request(method.clone(), &url)
        .header("Authorization", format!("Basic {}", auth))
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

        if is_united_states(&country) {
            if let Some(id_val) = &country.id {
                if let Some(id) = coerce_id(id_val) {
                    return Some(id); // Found it, break and return early
                }
            }
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
    let path = format!("contacts/edit/{}.json", cloudtalk_id);

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
    let customer = match load_customer_with_mapping(pool, customer_id).await.unwrap() {
        Some(customer) => customer,
        None => return NOT_FOUND_RESPONSE,
    };
    let Some(company_id) = customer.company_id else {
        return internal_error("The given user does not have a company");
    };
    if !(company_has_cloud_talk(pool, company_id).await.unwrap()) {
        return internal_error("Cloudtalk not configured for this company");
    }

    let us_country_id =
        get_cloudtalk_us_country_id(pool, client, company_id.try_into().unwrap()).await;
    let payload = build_payload(&customer, us_country_id);
    let Some(clean_payload) = payload else {
        return internal_error("Failed to build payload");
    };
    upsert_contact(
        pool,
        client,
        &customer,
        &clean_payload,
        company_id.try_into().unwrap(),
    )
    .await;
    OK_RESPONSE
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
    let path = format!("contacts/index.json?keyword={}&limit=10", encoded_phone);

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
        if extract_phones(&hit).iter().any(|phone| phone == e164_phone) {
            if let Some(id) = extract_id(&hit) {
                return Ok(Some(id)); // Return early with the found ID
            }
        }
    }

    Ok(None) // Return None if no matching contact was found
}

pub async fn find_cloudtalk_contact_by_phone(
    pool: &MySqlPool,
    client: &Client,
    company_id: u64,
    e164_phones: &[String],
) -> Result<Option<i32>, Box<dyn std::error::Error + Send + Sync>> {
    for phone in e164_phones {
        if let Some(id) = find_contact_by_one_phone(pool, client, company_id, phone).await? {
            return Ok(Some(id.try_into().unwrap()));
        }
    }
    Ok(None)
}
