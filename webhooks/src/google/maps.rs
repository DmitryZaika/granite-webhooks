use crate::google::schemas::{
    AutocompleteError, AutocompleteRequest, AutocompleteResponse, ComputeRouteMatrixRequest,
    DistanceError, FinalSuggestion, MatrixElement, PlaceDetailsResponse, RouteMatrixDestination,
    RouteMatrixOrigin, RouteModifiers, TextOrObject, Waypoint,
};
use lambda_http::tracing;
use reqwest::Client;

async fn generic_post_request<T, V>(
    url: &str,
    body: &T,
    field_mask: &str,
) -> Result<V, reqwest::Error>
where
    T: serde::Serialize + Send,
    V: serde::de::DeserializeOwned + Send,
{
    let client = Client::new();
    let api_key = std::env::var("GOOGLE_MAPS_API_KEY").expect("GOOGLE_MAPS_API_KEY must be set");
    client
        .post(url)
        .header("Content-Type", "application/json")
        .header("X-Goog-Api-Key", &api_key)
        .header("X-Goog-FieldMask", field_mask)
        .json(&body)
        .send()
        .await?
        .error_for_status()? // non-2xx -> error
        .json::<V>() // REST returns a JSON array of elements
        .await
}

async fn generic_get_request<V>(url: &str, field_mask: &str) -> Result<V, reqwest::Error>
where
    V: serde::de::DeserializeOwned,
{
    let client = Client::new();
    let api_key = std::env::var("GOOGLE_MAPS_API_KEY").expect("GOOGLE_MAPS_API_KEY must be set");
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

/// Возвращает расстояние по дороге в милях между origin и destination (самый короткий маршрут).
/// Использует Google Routes API Distance Matrix v2 и адресные строки.
pub async fn driving_distance_miles(origin: &str, destination: &str) -> Result<f64, DistanceError> {
    let body = ComputeRouteMatrixRequest {
        origins: vec![RouteMatrixOrigin {
            waypoint: Waypoint::new(origin),
            route_modifiers: Some(RouteModifiers::default()),
        }],
        destinations: vec![RouteMatrixDestination::new(destination)],
        travel_mode: "DRIVE".into(),
        routing_preference: "TRAFFIC_AWARE".into(),
    };

    let elements: Vec<MatrixElement> = generic_post_request(
        "https://routes.googleapis.com/distanceMatrix/v2:computeRouteMatrix",
        &body,
        "originIndex,destinationIndex,status,distanceMeters,condition,duration",
    )
    .await?;

    // Ask only for what we use; include condition/duration if you want them.

    // With 1x1 matrix we expect a single element; still pick [0,0] to be explicit.
    let el = elements
        .iter()
        .find(|e| e.origin_index == 0 && e.destination_index == 0)
        .ok_or(DistanceError::Shape)?;

    // Status OK if code == 0 (empty object => defaults to 0/"").
    if el.status_code() != 0 {
        return Err(DistanceError::Api(el.message()));
    }

    // Some responses also include a condition; ensure route exists.
    if let Some(cond) = &el.condition
        && cond != "ROUTE_EXISTS"
    {
        return Err(DistanceError::ElementStatus(cond.clone()));
    }

    let meters = el.distance_meters.ok_or(DistanceError::Shape)? as f64;
    Ok(meters / 1609.344_f64)
}

pub async fn get_address_autocomplete(
    query: &str,
) -> Result<Option<FinalSuggestion>, AutocompleteError> {
    let body = AutocompleteRequest::new(query);

    let response: AutocompleteResponse = generic_post_request(
        "https://places.googleapis.com/v1/places:autocomplete",
        &body,
        "suggestions.placePrediction.text,suggestions.placePrediction.placeId",
    )
    .await?;

    if response.suggestions.is_empty() {
        return Ok(None);
    }

    // Process suggestions sequentially, one at a time
    for s in response.suggestions {
        let place_id = s.place_prediction.place_id;

        let description_text = match s.place_prediction.text {
            TextOrObject::Object(pt) => pt.text,
            TextOrObject::String(str_val) => str_val,
        };

        let details_url = format!("https://places.googleapis.com/v1/places/{place_id}");

        if let Ok(address) =
            generic_get_request::<PlaceDetailsResponse>(&details_url, "addressComponents")
                .await
                .inspect_err(|err| tracing::error!("Failed to get address details: {:?}", err))
        {
            let parsed_address = address.to_parsed_address();

            // Validate that we got a real street and zip code
            if !parsed_address.street.is_empty() && parsed_address.zip.is_some() {
                // Short-circuit and return the single valid suggestion wrapped in Some
                return Ok(Some(FinalSuggestion::new(
                    description_text,
                    place_id,
                    parsed_address,
                )));
            }
        }
    }

    Ok(None)
}
