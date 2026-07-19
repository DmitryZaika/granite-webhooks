use crate::google::schemas::{
    AutocompleteError, ComputeRouteMatrixRequest, DistanceError, FinalSuggestion, GeocodeError,
    GeocodeResponse, LatLng, MatrixElement, RouteMatrixDestination, RouteMatrixOrigin,
    RouteModifiers, Waypoint,
};
use crate::google::utils::{
    fetch_autocomplete_suggestions, generic_post_request, process_single_suggestion,
};

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

/// Retrieves the FIRST valid address suggestion found, short-circuiting immediately.
pub async fn get_first_address_autocomplete(
    query: &str,
    bias_coords: Option<LatLng>,
) -> Result<Option<FinalSuggestion>, AutocompleteError> {
    let suggestions = fetch_autocomplete_suggestions(query, bias_coords).await?;

    for s in suggestions {
        if let Some(final_suggestion) = process_single_suggestion(s).await {
            return Ok(Some(final_suggestion));
        }
    }

    Ok(None)
}

/// Retrieves ALL valid address suggestions found from the query.
pub async fn get_all_address_autocompletes(
    query: &str,
    bias_coords: Option<LatLng>,
) -> Result<Vec<FinalSuggestion>, AutocompleteError> {
    let suggestions = fetch_autocomplete_suggestions(query, bias_coords).await?;
    let mut valid_suggestions = Vec::new();

    for s in suggestions {
        if let Some(final_suggestion) = process_single_suggestion(s).await {
            valid_suggestions.push(final_suggestion);
        }
    }

    Ok(valid_suggestions)
}

/// Converts a street address into latitude/longitude using the Google Geocoding API.
pub async fn geocode_address(address: &str) -> Result<LatLng, GeocodeError> {
    let api_key = std::env::var("GOOGLE_MAPS_API_KEY").expect("GOOGLE_MAPS_API_KEY must be set");
    let url = format!(
        "https://maps.googleapis.com/maps/api/geocode/json?address={}&key={}",
        urlencoding::encode(address),
        api_key
    );

    let response: GeocodeResponse = reqwest::get(&url).await?.json().await?;

    if response.status != "OK" {
        return Err(GeocodeError::Api(response.status));
    }

    let result = response
        .results
        .into_iter()
        .next()
        .ok_or(GeocodeError::NoResults)?;

    Ok(LatLng {
        latitude: result.geometry.location.lat,
        longitude: result.geometry.location.lng,
    })
}
