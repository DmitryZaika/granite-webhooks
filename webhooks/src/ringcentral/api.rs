use crate::crud::ringcentral::{
    company_has_ring_central, find_local_ringcentral_id_by_phone, get_access_token,
    load_customer_with_mapping, update_ringcentral_phone, upsert_ringcentral_mapping,
    CustomerWithMapping,
};
use crate::libs::constants::{NOT_FOUND_RESPONSE, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use crate::ringcentral::utils::{build_rc_contact_payload, normalize_to_e164, split_name};
use axum::http::StatusCode;
use lambda_http::tracing;
use reqwest::{Client, Method};
use serde::Serialize;
use serde_json::Value;
use sqlx::MySqlPool;

#[derive(Serialize)]
pub struct PublicRcContactPayload {
    #[serde(rename = "firstName")]
    pub first_name: String,
    #[serde(rename = "lastName")]
    pub last_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(rename = "mobilePhone", skip_serializing_if = "Option::is_none")]
    pub mobile_phone: Option<String>,
    #[serde(rename = "homePhone", skip_serializing_if = "Option::is_none")]
    pub home_phone: Option<String>,
    #[serde(rename = "businessPhone", skip_serializing_if = "Option::is_none")]
    pub business_phone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

async fn ringcentral_request(
    pool: &MySqlPool,
    client: &Client,
    company_id: u64,
    method: Method,
    path: &str,
    body: Option<&PublicRcContactPayload>,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let (token, server_url) = get_access_token(pool, company_id).await?;
    let url = format!(
        "{}/restapi/v1.0/{}",
        server_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let mut req = client
        .request(method, &url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json");
    if let Some(b) = body {
        req = req.json(b);
    }
    let response = req.send().await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "RingCentral API {status}: {}",
            text.chars().take(500).collect::<String>()
        )
        .into());
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_str(&text)?)
}

fn extract_contact_id(json: &Value) -> Option<u64> {
    json.get("id")
        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|n| n as u64)))
        .filter(|id| *id > 0)
}

pub async fn create_ringcentral_contact(
    pool: &MySqlPool,
    client: &Client,
    company_id: u64,
    payload: &PublicRcContactPayload,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let json = ringcentral_request(
        pool,
        client,
        company_id,
        Method::POST,
        "account/~/extension/~/address-book/contact",
        Some(payload),
    )
    .await?;
    extract_contact_id(&json).ok_or_else(|| "RingCentral create contact: missing id".into())
}

pub async fn update_ringcentral_contact(
    pool: &MySqlPool,
    client: &Client,
    company_id: u64,
    contact_id: u64,
    payload: &PublicRcContactPayload,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ringcentral_request(
        pool,
        client,
        company_id,
        Method::PUT,
        &format!("account/~/extension/~/address-book/contact/{contact_id}"),
        Some(payload),
    )
    .await?;
    Ok(())
}

async fn find_contact_by_phone(
    pool: &MySqlPool,
    client: &Client,
    company_id: u64,
    e164: &str,
) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
    let encoded = urlencoding::encode(e164);
    let json = ringcentral_request(
        pool,
        client,
        company_id,
        Method::GET,
        &format!("account/~/extension/~/address-book/contact?phoneNumber={encoded}&perPage=10"),
        None,
    )
    .await?;
    let records = json
        .get("records")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for record in records {
        if let Some(id) = extract_contact_id(&record) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

async fn upsert_contact(
    pool: &MySqlPool,
    client: &Client,
    mapping: &CustomerWithMapping,
    payload: &PublicRcContactPayload,
    company_id: u64,
) {
    let phones: Vec<String> = [&mapping.phone, &mapping.phone_2]
        .iter()
        .filter_map(|p| normalize_to_e164(p.as_deref()))
        .collect();

    let existing_id = if let Some(id) = mapping.ringcentral_id {
        Some(id as u64)
    } else if let Ok(Some(local)) =
        find_local_ringcentral_id_by_phone(pool, company_id, &phones).await
    {
        Some(local as u64)
    } else {
        let mut found = None;
        for phone in &phones {
            if let Ok(Some(id)) = find_contact_by_phone(pool, client, company_id, phone).await {
                found = Some(id);
                break;
            }
        }
        found
    };

    let result = if let Some(contact_id) = existing_id {
        match update_ringcentral_contact(pool, client, company_id, contact_id, payload).await {
            Ok(()) => Ok(contact_id),
            Err(e) => Err(e),
        }
    } else {
        create_ringcentral_contact(pool, client, company_id, payload).await
    };

    match result {
        Ok(contact_id) => {
            let phone1 = phones.first().cloned();
            let phone2 = phones.get(1).cloned();
            if let Some(mapping_row_id) = mapping.ringcentral_contact_id {
                let _ = update_ringcentral_phone(
                    pool,
                    phone1.clone(),
                    phone2.clone(),
                    i64::from(mapping_row_id),
                )
                .await;
            }
            if let Err(error) = upsert_ringcentral_mapping(
                pool,
                mapping.id,
                company_id as i32,
                contact_id as i64,
                phone1,
                phone2,
            )
            .await
            {
                tracing::error!(
                    ?error,
                    customer_id = mapping.id,
                    "Failed to upsert ringcentral_contacts"
                );
            }
        }
        Err(error) => {
            tracing::error!(
                ?error,
                customer_id = mapping.id,
                "Failed to sync RingCentral contact"
            );
        }
    }
}

pub async fn sync_customer_to_ring_central(
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
        Ok(id) => id,
        Err(error) => {
            tracing::error!(?error, company_id, "Failed to convert company ID");
            return internal_error("Failed to convert company ID");
        }
    };
    match company_has_ring_central(pool, company_id).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::UNAUTHORIZED,
                "RingCentral not configured for this company",
            );
        }
        Err(error) => {
            tracing::error!(
                ?error,
                company_id,
                "Failed to check RingCentral configuration"
            );
            return internal_error("Failed to check RingCentral configuration");
        }
    }

    let (first_name, last_name) = split_name(mapping.name.as_deref());
    let payload = build_rc_contact_payload(&mapping, first_name, last_name);
    upsert_contact(pool, client, &mapping, &payload, clean_company_id).await;
    OK_RESPONSE
}
