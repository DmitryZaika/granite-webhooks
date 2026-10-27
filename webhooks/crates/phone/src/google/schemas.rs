use crate::cloudtalk::schemas::ParsedAddress;
use serde::{Deserialize, Serialize};

/// Longest response body kept on a non-success Google API status, in characters.
const MAX_ERROR_BODY_CHARS: usize = 500;

/// Error from a `generic_post_request` call.
///
/// Either a transport failure, or a non-success HTTP status, in which case
/// the (truncated) response body is kept so the actual Google error message
/// survives instead of being discarded by `error_for_status`.
#[derive(thiserror::Error, Debug)]
pub enum GoogleApiError {
    #[error("Network error: {0}")]
    Net(#[from] reqwest::Error),
    #[error("Google API error: status {status}, body: {body}")]
    Status { status: u16, body: String },
}

impl GoogleApiError {
    pub fn status(status: u16, body: impl AsRef<str>) -> Self {
        let body: String = body.as_ref().chars().take(MAX_ERROR_BODY_CHARS).collect();
        Self::Status { status, body }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum DistanceError {
    #[error("Google API error: {0}")]
    Api(String),
    #[error("Element status/condition: {0}")]
    ElementStatus(String),
    #[error("Network: {0}")]
    Net(#[from] reqwest::Error),
    #[error("Unexpected response shape")]
    Shape,
    #[error(transparent)]
    GoogleApi(#[from] GoogleApiError),
}

// google.rpc.Status
#[derive(Deserialize, Debug, Default)]
struct RpcStatus {
    #[serde(default)]
    code: i32, // 0 == OK
    #[serde(default)]
    message: String,
    // details omitted
}

// --- Routes API v2 response shape (only the fields we request via FieldMask) ---
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MatrixElement {
    pub origin_index: i32,
    pub destination_index: i32,

    #[serde(default)]
    status: RpcStatus, // object, not a string

    #[serde(default)]
    pub distance_meters: Option<i64>,

    // These can appear; keep them optional.
    // #[serde(default)]
    // duration: Option<String>, // e.g., "160s"
    #[serde(default)]
    pub condition: Option<String>, // e.g., "ROUTE_EXISTS"
}

impl MatrixElement {
    pub const fn status_code(&self) -> i32 {
        self.status.code
    }

    pub fn message(&self) -> String {
        let msg = self.status.message.clone();
        if msg.is_empty() {
            "OK".to_string()
        } else {
            msg
        }
    }
}

// --- Request body types (minimal) ---
#[derive(Serialize)]
pub struct Waypoint {
    address: String,
}

impl Waypoint {
    pub fn new(address: &str) -> Self {
        Self {
            address: address.to_string(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteModifiers {
    #[serde(skip_serializing_if = "Option::is_none")]
    avoid_ferries: Option<bool>,
}

impl Default for RouteModifiers {
    fn default() -> Self {
        Self {
            avoid_ferries: Some(false),
        }
    }
}

#[derive(Serialize)]
pub struct RouteMatrixOrigin {
    pub waypoint: Waypoint,
    #[serde(rename = "routeModifiers", skip_serializing_if = "Option::is_none")]
    pub route_modifiers: Option<RouteModifiers>,
}

#[derive(Serialize)]
pub struct RouteMatrixDestination {
    waypoint: Waypoint,
}

impl RouteMatrixDestination {
    pub fn new(address: &str) -> Self {
        Self {
            waypoint: Waypoint::new(address),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputeRouteMatrixRequest {
    pub origins: Vec<RouteMatrixOrigin>,
    pub destinations: Vec<RouteMatrixDestination>,
    pub travel_mode: String,        // "DRIVE"
    pub routing_preference: String, // "TRAFFIC_AWARE" or "TRAFFIC_UNAWARE"
}

#[derive(thiserror::Error, Debug)]
pub enum AutocompleteError {
    #[error("Network error: {0}")]
    Net(#[from] reqwest::Error),
    #[error("API configuration error: {0}")]
    Config(String),
    #[error(transparent)]
    GoogleApi(#[from] GoogleApiError),
}

// --- Request body types ---
#[derive(Serialize, Clone, Copy)]
pub struct LatLng {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Serialize)]
pub struct Circle {
    pub center: LatLng,
    pub radius: f64, // Radius in meters (e.g., 10000.0 for 10km)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationBias {
    pub circle: Circle,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutocompleteRequest {
    pub input: String,
    pub language_code: String,
    pub included_region_codes: Vec<String>,
    // Use skip_serializing_if so it won't break requests when coordinates aren't supplied
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_bias: Option<LocationBias>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<LatLng>,
}

impl AutocompleteRequest {
    pub fn new(address: &str) -> Self {
        // Добав: , bias_coords: Option<LatLng>
        /*
        let location_bias = bias_coords.map(|coords| LocationBias {
            circle: Circle {
                center: coords,
                radius: 100000.0, // 100 km
            },
        });
        */

        Self {
            input: address.to_string(),
            language_code: "en".into(),
            included_region_codes: vec!["US".into()],
            location_bias: None,
            origin: None,
        }
    }
}

// --- Autocomplete API v1 response shapes ---
#[derive(Deserialize, Debug)]
pub struct PredictionText {
    pub text: String,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum TextOrObject {
    Object(PredictionText),
    String(String),
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PlacePrediction {
    pub text: TextOrObject,
    pub place_id: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    pub place_prediction: PlacePrediction,
}

#[derive(Deserialize, Debug, Default)]
pub struct AutocompleteResponse {
    #[serde(default)]
    pub suggestions: Vec<Suggestion>,
}

// --- Place Details API v1 response shapes ---
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AddressComponent {
    pub long_text: String,
    pub short_text: String,
    pub types: Vec<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PlaceDetailsResponse {
    pub address_components: Option<Vec<AddressComponent>>,
}

impl PlaceDetailsResponse {
    pub fn to_parsed_address(&self) -> ParsedAddress {
        let mut street_number = String::new();
        let mut route = String::new();
        let mut city = None;
        let mut state = None;
        let mut zip = None;

        if let Some(components) = &self.address_components {
            for component in components {
                // Google component types can have multiple entries, so check via .contains()
                if component.types.contains(&"street_number".to_string()) {
                    street_number.clone_from(&component.long_text);
                } else if component.types.contains(&"route".to_string()) {
                    route.clone_from(&component.long_text);
                } else if component.types.contains(&"locality".to_string()) {
                    city = Some(component.long_text.clone());
                } else if component
                    .types
                    .contains(&"administrative_area_level_1".to_string())
                {
                    // .short_text gives the 2-letter code (e.g., "CA"), .long_text gives "California"
                    state = Some(component.short_text.clone());
                } else if component.types.contains(&"postal_code".to_string()) {
                    zip = Some(component.long_text.clone());
                }
            }
        }

        // Combine street number and route cleanly (handles missing street numbers or route-only entries)
        let street = format!("{street_number} {route}").trim().to_string();

        ParsedAddress {
            street,
            city,
            state,
            zip,
        }
    }
}

// --- Final output types returned by the handler ---
#[derive(Serialize, Debug)]
pub struct Description {
    pub text: String,
}

impl Description {
    pub const fn new(text: String) -> Self {
        Self { text }
    }
}

#[derive(Serialize, Debug)]
pub struct FinalSuggestion {
    pub description: Description,
    pub place_id: String,
    pub address: ParsedAddress,
}

impl FinalSuggestion {
    pub const fn new(description: String, place_id: String, address: ParsedAddress) -> Self {
        Self {
            description: Description::new(description),
            place_id,
            address,
        }
    }
}

#[derive(Deserialize)]
pub struct AddressRequest {
    pub query: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_api_error_display_and_debug_include_status_and_body() {
        let error = GoogleApiError::status(400, "INVALID_ARGUMENT: API key not valid");

        let display = format!("{error}");
        assert!(display.contains("400"));
        assert!(display.contains("API key not valid"));

        let debug = format!("{error:?}");
        assert!(debug.contains("400"));
        assert!(debug.contains("API key not valid"));
    }

    #[test]
    fn google_api_error_truncates_long_body() {
        let long_body = "x".repeat(600);
        let error = GoogleApiError::status(400, &long_body);
        match error {
            GoogleApiError::Status { body, .. } => assert_eq!(body.len(), MAX_ERROR_BODY_CHARS),
            GoogleApiError::Net(_) => panic!("expected Status variant"),
        }
    }

    #[test]
    fn autocomplete_request_serializes_expected_body() {
        let request = AutocompleteRequest::new("123 Main St, Springfield, IL");
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"input":"123 Main St, Springfield, IL","languageCode":"en","includedRegionCodes":["US"]}"#
        );
    }
}
