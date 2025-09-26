use reqwest::Client;
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
struct MatrixElement {
    origin_index: i32,
    destination_index: i32,

    #[serde(default)]
    status: RpcStatus, // object, not a string

    #[serde(default)]
    distance_meters: Option<i64>,

    // These can appear; keep them optional.
    #[serde(default)]
    duration: Option<String>, // e.g., "160s"
    #[serde(default)]
    condition: Option<String>, // e.g., "ROUTE_EXISTS"
}

// --- Request body types (minimal) ---
#[derive(Serialize)]
struct Waypoint {
    address: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RouteModifiers {
    #[serde(skip_serializing_if = "Option::is_none")]
    avoid_ferries: Option<bool>,
}

#[derive(Serialize)]
struct RouteMatrixOrigin {
    waypoint: Waypoint,
    #[serde(skip_serializing_if = "Option::is_none")]
    routeModifiers: Option<RouteModifiers>, // keep your exact field name if you prefer
}

#[derive(Serialize)]
struct RouteMatrixDestination {
    waypoint: Waypoint,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ComputeRouteMatrixRequest {
    origins: Vec<RouteMatrixOrigin>,
    destinations: Vec<RouteMatrixDestination>,
    travel_mode: String,        // "DRIVE"
    routing_preference: String, // "TRAFFIC_AWARE" or "TRAFFIC_UNAWARE"
}

/// Возвращает расстояние по дороге в милях между origin и destination (самый короткий маршрут).
/// Использует Google Routes API Distance Matrix v2 и адресные строки.
pub async fn driving_distance_miles(origin: &str, destination: &str) -> Result<f64, DistanceError> {
    let client = Client::new();
    let api_key = std::env::var("GOOGLE_MAPS_API_KEY").expect("GOOGLE_MAPS_API_KEY must be set");

    let body = ComputeRouteMatrixRequest {
        origins: vec![RouteMatrixOrigin {
            waypoint: Waypoint {
                address: origin.to_string(),
            },
            routeModifiers: Some(RouteModifiers {
                avoid_ferries: Some(false),
            }),
        }],
        destinations: vec![RouteMatrixDestination {
            waypoint: Waypoint {
                address: destination.to_string(),
            },
        }],
        travel_mode: "DRIVE".into(),
        routing_preference: "TRAFFIC_AWARE".into(),
    };

    // Ask only for what we use; include condition/duration if you want them.
    let elements = client
        .post("https://routes.googleapis.com/distanceMatrix/v2:computeRouteMatrix")
        .header("Content-Type", "application/json")
        .header("X-Goog-Api-Key", &api_key)
        .header(
            "X-Goog-FieldMask",
            "originIndex,destinationIndex,status,distanceMeters,condition,duration",
        )
        .json(&body)
        .send()
        .await?
        .error_for_status()? // non-2xx -> error
        .json::<Vec<MatrixElement>>() // REST returns a JSON array of elements
        .await?;

    // With 1x1 matrix we expect a single element; still pick [0,0] to be explicit.
    let el = elements
        .iter()
        .find(|e| e.origin_index == 0 && e.destination_index == 0)
        .ok_or(DistanceError::Shape)?;

    // Status OK if code == 0 (empty object => defaults to 0/"").
    if el.status.code != 0 {
        return Err(DistanceError::Api(if el.status.message.is_empty() {
            "non-OK status".into()
        } else {
            el.status.message.clone()
        }));
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
