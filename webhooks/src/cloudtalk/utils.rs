use crate::cloudtalk::api::{
    create_cloudtalk_contact, find_cloudtalk_contact_by_phone, update_cloudtalk_contact,
};
use crate::cloudtalk::schemas::{CloudTalkCountry, ContactSearchHit};
use crate::crud::cloudtalk::CustomerWithMapping;
use crate::crud::cloudtalk::{find_local_cloudtalk_id_by_phone, update_cloudtalk_phone};
use crate::google::maps::get_first_address_autocomplete;
use crate::libs::constants::OK_RESPONSE;
use crate::libs::types::BasicResponse;
use lambda_http::tracing;
use reqwest::Client;
use sqlx::MySqlPool;
use std::env;

// Bring in the structs from the previous step
use crate::cloudtalk::schemas::{ContactEmail, ContactNumber, ContactPayload, ExternalUrl};

pub fn is_united_states(country: &CloudTalkCountry) -> bool {
    let iso_match = country
        .iso_code
        .as_deref()
        .or(country.iso.as_deref())
        .or(country.code.as_deref())
        .is_some_and(|code| code == "US" || code == "USA");

    let name_match = country
        .name
        .as_deref()
        .is_some_and(|name| name == "United States");

    iso_match || name_match
}

pub fn coerce_id(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(n) => {
            let val = n.as_u64()?;
            if val > 0 { Some(val) } else { None }
        }
        serde_json::Value::String(s) => {
            let val = s.parse::<u64>().ok()?;
            if val > 0 { Some(val) } else { None }
        }
        _ => None,
    }
}

// --- Lazy Static Globals ---

const US_STATES: [&str; 56] = [
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA", "HI", "ID", "IL", "IN", "IA", "KS",
    "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM", "NY",
    "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA", "WV",
    "WI", "WY", "DC", "PR", "VI", "GU", "AS", "MP",
];

pub fn phone_digits_only(phone: &str) -> String {
    phone.chars().filter(char::is_ascii_digit).collect()
}

pub fn normalize_to_e164(phone: Option<&str>) -> Option<String> {
    let trimmed = phone?.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.starts_with('+') {
        return Some(format!("+{}", phone_digits_only(trimmed)));
    }

    let digits = phone_digits_only(trimmed);
    if digits.len() == 10 {
        return Some(format!("+1{digits}"));
    }
    if digits.len() == 11 && digits.starts_with('1') {
        return Some(format!("+{digits}"));
    }

    None
}

pub fn build_phones(customer: &CustomerWithMapping) -> Vec<ContactNumber> {
    let mut phones = Vec::new();

    // Iterate over the two optional phone fields safely
    for raw_phone in [&customer.phone, &customer.phone_2] {
        if let Some(e164) = normalize_to_e164(raw_phone.as_deref()) {
            phones.push(ContactNumber {
                public_number: e164,
            });
        }
    }

    phones
}

// --- Email & URL Helpers ---

pub fn build_emails(customer: &CustomerWithMapping) -> Vec<ContactEmail> {
    match &customer.email {
        Some(email) => {
            let trimmed = email.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![ContactEmail {
                    email: trimmed.to_string(),
                }]
            }
        }
        None => Vec::new(),
    }
}

pub fn build_external_urls(customer: &CustomerWithMapping) -> Vec<ExternalUrl> {
    // Looks up APP_URL at runtime from environment variables
    match env::var("APP_URL") {
        Ok(app_url) if !app_url.is_empty() => vec![ExternalUrl {
            name: "Granite Manager".to_string(),
            url: format!("{}/employee/customers/info/{}/info", app_url, customer.id),
        }],
        _ => Vec::new(),
    }
}

// --- Address Parsing Helpers ---

pub fn is_us_state(code: &str) -> bool {
    US_STATES.contains(&code)
}

pub async fn build_payload(
    customer: &CustomerWithMapping,
    us_country_id: Option<u64>,
) -> Option<ContactPayload> {
    let numbers = build_phones(customer);
    if numbers.is_empty() {
        return None;
    }

    let external_urls = build_external_urls(customer);

    let external_url = if external_urls.is_empty() {
        None
    } else {
        Some(external_urls)
    };

    // Initialize payload with required fields
    let mut payload = ContactPayload {
        name: customer.name.clone(),
        contact_number: numbers,
        contact_email: build_emails(customer),
        external_url,
        ..Default::default() // Sets the remaining Option fields to None
    };

    let Some(customer_address) = &customer.address else {
        return Some(payload);
    };
    let parsed = match get_first_address_autocomplete(customer_address, None).await {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            tracing::error!(
                "No valid address found for customer address: {}",
                customer_address
            );
            return Some(payload);
        }
        Err(error) => {
            tracing::error!(
                "Failed to parse address for customer address: {}, error: {}",
                customer_address,
                error
            );
            return Some(payload);
        }
    };
    payload.address = Some(parsed.address.street);
    if parsed.address.city.is_some() {
        payload.city = parsed.address.city;
    }
    if parsed.address.state.is_some() {
        payload.state = parsed.address.state.clone();
    }
    if parsed.address.zip.is_some() {
        payload.zip = parsed.address.zip.clone();
    }

    // Check condition: if state, zip, and us_country_id are all present
    if parsed.address.state.is_some() && parsed.address.zip.is_some() && us_country_id.is_some() {
        payload.country_id = us_country_id;
    }

    Some(payload)
}

pub async fn upsert_contact(
    pool: &MySqlPool,
    client: &Client,
    customer: &CustomerWithMapping,
    payload: &ContactPayload,
    company_id: u64,
) -> BasicResponse {
    let phone_array: Vec<String> = payload
        .contact_number
        .iter()
        .map(|n| n.public_number.clone())
        .collect();

    let phone1 = phone_array.first().cloned();
    let phone2 = phone_array.get(1).cloned();

    if let Some(cloudtalk_id) = customer.cloudtalk_id
        && let Some(_cloudtalk_contact_id) = customer.cloudtalk_contact_id
    {
        update_cloudtalk_contact(pool, client, company_id, cloudtalk_id, payload)
            .await
            .unwrap();

        update_cloudtalk_phone(pool, phone1, phone2, cloudtalk_id)
            .await
            .unwrap();

        return OK_RESPONSE;
    }

    let mut existing_id = find_local_cloudtalk_id_by_phone(pool, company_id, &phone_array)
        .await
        .unwrap();

    if existing_id.is_none() {
        existing_id = find_cloudtalk_contact_by_phone(pool, client, company_id, &phone_array)
            .await
            .unwrap();
    }

    if let Some(id) = existing_id {
        update_cloudtalk_contact(pool, client, company_id, id.into(), payload)
            .await
            .unwrap();
    }

    let cloudtalk_id: u64 = match existing_id {
        Some(id) => id.try_into().unwrap(),
        None => create_cloudtalk_contact(pool, client, company_id, payload)
            .await
            .unwrap(),
    };

    sqlx::query!(
        "INSERT INTO cloudtalk_contacts (customer_id, company_id, cloudtalk_id, phone_e164_1, phone_e164_2)
         VALUES (?, ?, ?, ?, ?)",
        customer.id,
        customer.company_id,
        cloudtalk_id,
        phone1,
        phone2
    )
    .execute(pool)
    .await.unwrap();

    OK_RESPONSE
}

pub fn find_contact_id(json: &serde_json::Value) -> Option<u64> {
    // If !json or typeof json !== 'object'
    if !json.is_object() {
        return None;
    }

    // const responseData = obj.responseData
    let response_data = json.get("responseData");

    // const data = responseData?.data ?? obj.data
    let data = response_data
        .and_then(|rd| rd.get("data"))
        .or_else(|| json.get("data"));

    // Collect candidates sequentially to preserve the original evaluation priority
    let mut candidates = Vec::new();

    if let Some(d) = data {
        // (data?.Contact)?.id
        if let Some(contact) = d.get("Contact")
            && let Some(id) = contact.get("id")
        {
            candidates.push(id);
        }
        // data?.id
        if let Some(id) = d.get("id") {
            candidates.push(id);
        }
    }

    // (obj.Contact)?.id
    if let Some(contact) = json.get("Contact")
        && let Some(id) = contact.get("id")
    {
        candidates.push(id);
    }

    // obj.id
    if let Some(id) = json.get("id") {
        candidates.push(id);
    }

    // For the candidates, attempt to coerce them into an integer ID
    for c in candidates {
        if let Some(id) = coerce_id(c) {
            return Some(id);
        }
    }

    None
}

pub fn extract_phones(hit: &ContactSearchHit) -> Vec<String> {
    // Replaces: const node = hit.Contact ?? hit
    let (contact_numbers, contact_number_list) = match &hit.contact {
        Some(node) => (&node.contact_numbers, &node.contact_number),
        None => (&hit.contact_numbers, &hit.contact_number),
    };

    let mut phones = Vec::new();

    // Loop 1: node.contact_numbers ?? []
    if let Some(numbers) = contact_numbers {
        for p in numbers {
            // In TS you checked `typeof p === 'string'`.
            // In Rust, the type system guarantees it's a String.
            if let Some(normalized_phone) = normalize_to_e164(Some(p)) {
                phones.push(normalized_phone);
            }
        }
    }

    // Loop 2: node.ContactNumber ?? []
    if let Some(objs) = contact_number_list {
        for p in objs {
            // Replaces: if (p?.public_number)
            if let Some(ref pub_num) = p.public_number
                && !pub_num.is_empty()
            {
                // Mimics JS truthiness check
                if let Some(normalized_phone) = normalize_to_e164(Some(pub_num)) {
                    phones.push(normalized_phone);
                }
            }
        }
    }

    phones
}

/// Extracts the ID from the contact or root hit and passes it to `coerce_id`.
pub fn extract_id(hit: &ContactSearchHit) -> Option<u64> {
    // Replaces: hit.Contact?.id ?? hit.id
    let raw_id = hit
        .contact
        .as_ref()
        .and_then(|c| c.id.as_ref())
        .or(hit.id.as_ref());

    raw_id.and_then(super::schemas::ContactId::coerce)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn build_payload_basic() {
        let mapping = CustomerWithMapping {
            id: 1,
            company_id: Some(2),
            name: Some("Test Company".to_string()),
            phone: Some("+11234567890".to_string()),
            phone_2: None,
            email: None,
            address: Some("2001 E Greyhound Pass C07, Carmel, IN 46033".to_string()),
            cloudtalk_contact_id: None,
            cloudtalk_id: None,
        };
        let result = ContactPayload {
            name: Some("Test Company".to_string()),
            contact_number: vec![ContactNumber {
                public_number: "+11234567890".to_string(),
            }],
            contact_email: vec![],
            external_url: None,
            address: Some("2001 East Greyhound Pass".to_string()),
            city: Some("Carmel".to_string()),
            state: Some("IN".to_string()),
            zip: Some("46033".to_string()),
            country_id: Some(13),
        };
        let payload = build_payload(&mapping, Some(13)).await.unwrap();
        assert_eq!(payload, result);
    }
}
