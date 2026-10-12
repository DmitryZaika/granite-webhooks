//! Google Maps: driving distances. Configured with `GOOGLE_MAPS_API_KEY`;
//! without it every distance is unknown.

use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ROUTE_MATRIX_URL: &str = "https://routes.googleapis.com/distanceMatrix/v2:computeRouteMatrix";
const METERS_PER_MILE: f64 = 1609.344;

#[derive(Clone)]
pub struct Maps {
    key: Option<Arc<str>>,
    url: Arc<str>,
    http: reqwest::Client,
    /// Miles already asked for, so reopening a quote does not ask Google again.
    cache: Arc<Mutex<HashMap<String, Option<f64>>>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatrixElement {
    condition: Option<String>,
    distance_meters: Option<f64>,
}

impl Maps {
    pub fn disabled() -> Self {
        Self::new(None, ROUTE_MATRIX_URL)
    }

    pub fn from_env() -> Self {
        Self::new(
            std::env::var("GOOGLE_MAPS_API_KEY").ok().as_deref(),
            ROUTE_MATRIX_URL,
        )
    }

    /// `url` is the route matrix endpoint (a stub in tests).
    pub fn new(key: Option<&str>, url: &str) -> Self {
        Self {
            key: key
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(Arc::from),
            url: Arc::from(url),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("HTTP client builds"),
            cache: Arc::default(),
        }
    }

    /// Driving miles (one decimal) from `from` to `to`, or `None` when there
    /// is no route, no key, or Google fails.
    pub async fn driving_miles(&self, from: &str, to: &str) -> Option<f64> {
        let key = self.key.as_ref()?;
        let cache_key = format!(
            "{}|{}",
            from.trim().to_lowercase(),
            to.trim().to_lowercase()
        );
        if let Some(hit) = self.cache.lock().ok()?.get(&cache_key) {
            return *hit;
        }
        let response = self
            .http
            .post(&*self.url)
            .header("X-Goog-Api-Key", &**key)
            .header("X-Goog-FieldMask", "distanceMeters,condition")
            .json(&json!({
                "origins": [{ "waypoint": { "address": from } }],
                "destinations": [{ "waypoint": { "address": to } }],
                "travelMode": "DRIVE",
            }))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let elements: Vec<MatrixElement> = response.json().await.ok()?;
        let miles = elements
            .first()
            .filter(|element| element.condition.as_deref() == Some("ROUTE_EXISTS"))
            .and_then(|element| element.distance_meters)
            .map(|meters| (meters / METERS_PER_MILE * 10.0).round() / 10.0);
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(cache_key, miles);
        }
        miles
    }
}
