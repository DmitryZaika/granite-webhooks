use crate::cloudtalk::schemas::ParsedAddress;
use serde::{Deserialize, Serialize};

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
}

// --- Request body types ---
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutocompleteRequest {
    pub input: String,
    pub language_code: String,
    pub included_region_codes: Vec<String>,
}

impl AutocompleteRequest {
    pub fn new(address: &str) -> Self {
        Self {
            input: address.to_string(),
            language_code: "en".into(),
            included_region_codes: vec!["US".into()],
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
