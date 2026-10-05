use crate::google::schemas::{
    AutocompleteError, AutocompleteRequest, AutocompleteResponse, FinalSuggestion, GoogleApiError,
    PlaceDetailsResponse, Suggestion, TextOrObject,
};
use lambda_http::tracing;
use reqwest::Client;

const PLACES_BASE_URL: &str = "https://places.googleapis.com/v1";

/// Places API root and the key to send. Tests that installed a loopback stub
/// get it, with a placeholder key, so they need neither network nor secrets.
fn places_endpoint() -> (String, String) {
    #[cfg(test)]
    if let Some(stub) = crate::tests::stub_http::google_places_base_url() {
        return (stub, "test-key".to_string());
    }
    (
        PLACES_BASE_URL.to_string(),
        std::env::var("GOOGLE_MAPS_API_KEY").expect("GOOGLE_MAPS_API_KEY must be set"),
    )
}
/// Common function to handle the initial autocomplete API post request.
///
/// On a non-success status, the response body is read (and truncated) so the
/// caller can see Google's actual error message instead of it being
/// discarded by `error_for_status`.
pub async fn generic_post_request<T, V>(
    url: &str,
    body: &T,
    field_mask: &str,
) -> Result<V, GoogleApiError>
where
    T: serde::Serialize + Send + Sync,
    V: serde::de::DeserializeOwned + Send,
{
    let client = Client::new();
    let api_key = places_endpoint().1;
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("X-Goog-Api-Key", &api_key)
        .header("X-Goog-FieldMask", field_mask)
        .json(body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        return Err(GoogleApiError::status(status.as_u16(), body_text));
    }

    Ok(response.json::<V>().await?)
}

async fn generic_get_request<V>(url: &str, field_mask: &str) -> Result<V, reqwest::Error>
where
    V: serde::de::DeserializeOwned,
{
    let client = Client::new();
    let api_key = places_endpoint().1;
    client
        .get(url)
        .header("Content-Type", "application/json")
        .header("X-Goog-Api-Key", &api_key)
        .header("X-Goog-FieldMask", field_mask)
        .send()
        .await?
        .error_for_status()? // non-2xx -> error
        .json::<V>() // REST returns a JSON array of elements
        .await
}
pub async fn fetch_autocomplete_suggestions(
    query: &str,
) -> Result<Vec<Suggestion>, AutocompleteError> {
    let body = AutocompleteRequest::new(query);

    let response: AutocompleteResponse = generic_post_request(
        &format!("{}/places:autocomplete", places_endpoint().0),
        &body,
        "suggestions.placePrediction.text,suggestions.placePrediction.placeId",
    )
    .await?;

    Ok(response.suggestions)
}

/// Common function to process, fetch details for, and validate an individual suggestion.
pub async fn process_single_suggestion(s: Suggestion) -> Option<FinalSuggestion> {
    let place_id = s.place_prediction.place_id;

    let description_text = match s.place_prediction.text {
        TextOrObject::Object(pt) => pt.text,
        TextOrObject::String(str_val) => str_val,
    };

    let details_url = format!("{}/places/{place_id}", places_endpoint().0);

    // Fetch place details; returns None early if the HTTP call fails
    let address = generic_get_request::<PlaceDetailsResponse>(&details_url, "addressComponents")
        .await
        .inspect_err(|err| tracing::error!("Failed to get address details: {:?}", err))
        .ok()?;

    let parsed_address = address.to_parsed_address();

    // Validate that we got a real street and zip code
    if !parsed_address.street.is_empty() && parsed_address.zip.is_some() {
        Some(FinalSuggestion::new(
            description_text,
            place_id,
            parsed_address,
        ))
    } else {
        None
    }
}
