//! Google Maps: driving distances and times, geocoding and static maps.
//!
//! Configured with `GOOGLE_MAPS_API_KEY`; without it every distance is
//! unknown and no map is drawn.

use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ROUTE_MATRIX_URL: &str = "https://routes.googleapis.com/distanceMatrix/v2:computeRouteMatrix";
const MAPS_API_URL: &str = "https://maps.googleapis.com/maps/api";
/// Most stops drawn on a day map.
const MAX_MAP_STOPS: usize = 20;
const HOME_ICON: &str = "https://maps.gstatic.com/mapfiles/ms2/micons/homegardenbusiness.png";

/// A geocoded point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub lat: f64,
    pub lng: f64,
}
const METERS_PER_MILE: f64 = 1609.344;

#[derive(Clone)]
pub struct Maps {
    key: Option<Arc<str>>,
    url: Arc<str>,
    api_base: Arc<str>,
    http: reqwest::Client,
    /// Miles already asked for, so reopening a quote does not ask Google again.
    cache: Arc<Mutex<HashMap<String, Option<f64>>>>,
    /// Addresses already geocoded.
    points: Arc<Mutex<HashMap<String, Option<Point>>>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatrixElement {
    condition: Option<String>,
    distance_meters: Option<f64>,
}

impl Maps {
    pub fn disabled() -> Self {
        Self::new(None, ROUTE_MATRIX_URL, MAPS_API_URL)
    }

    pub fn from_env() -> Self {
        Self::new(
            std::env::var("GOOGLE_MAPS_API_KEY").ok().as_deref(),
            ROUTE_MATRIX_URL,
            MAPS_API_URL,
        )
    }

    /// `url` is the route matrix endpoint and `api_base` the base of the
    /// geocode and static map APIs (stubs in tests).
    pub fn new(key: Option<&str>, url: &str, api_base: &str) -> Self {
        Self {
            key: key
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(Arc::from),
            url: Arc::from(url),
            api_base: Arc::from(api_base.trim_end_matches('/')),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("HTTP client builds"),
            cache: Arc::default(),
            points: Arc::default(),
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

/// `base?k=v&…` with every key and value percent-encoded.
fn with_query(base: &str, pairs: &[(&str, &str)]) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    let query: Vec<String> = pairs
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                utf8_percent_encode(key, NON_ALPHANUMERIC),
                utf8_percent_encode(value, NON_ALPHANUMERIC)
            )
        })
        .collect();
    format!("{base}?{}", query.join("&"))
}

/// Seconds of a Google duration like `"754s"`.
fn seconds(duration: &Value) -> Option<f64> {
    duration.as_str()?.strip_suffix('s')?.parse().ok()
}

impl Maps {
    /// Where an address is, or `None` when Google cannot find it.
    pub async fn geocode(&self, address: &str) -> Option<Point> {
        let key = self.key.as_ref()?;
        let cache_key = address.trim().to_lowercase();
        if cache_key.is_empty() {
            return None;
        }
        if let Some(hit) = self.points.lock().ok()?.get(&cache_key) {
            return *hit;
        }
        let body: Option<Value> = async {
            self.http
                .get(with_query(
                    &format!("{}/geocode/json", self.api_base),
                    &[("address", address.trim()), ("key", key)],
                ))
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?
                .json()
                .await
                .ok()
        }
        .await;
        let point = body.and_then(|body| {
            if body["status"] != "OK" {
                return None;
            }
            let location = &body["results"][0]["geometry"]["location"];
            Some(Point {
                lat: location["lat"].as_f64()?,
                lng: location["lng"].as_f64()?,
            })
        });
        if let Ok(mut points) = self.points.lock() {
            points.insert(cache_key, point);
        }
        point
    }

    /// A PNG map (as a `data:` URL) of the home address and up to 20 stops
    /// joined in order; `None` when nothing could be placed.
    pub async fn day_map(&self, home: Option<&str>, stops: &[String]) -> Option<String> {
        let key = self.key.as_ref()?;
        let addresses: Vec<&str> = stops
            .iter()
            .map(|stop| stop.trim())
            .filter(|stop| !stop.is_empty())
            .take(MAX_MAP_STOPS)
            .collect();
        if addresses.is_empty() {
            return None;
        }
        let home_point = match home.map(str::trim).filter(|home| !home.is_empty()) {
            Some(home) => self.geocode(home).await,
            None => None,
        };
        let mut stop_points = Vec::new();
        for address in addresses {
            if let Some(point) = self.geocode(address).await {
                stop_points.push(point);
            }
        }
        if home_point.is_none() && stop_points.is_empty() {
            return None;
        }
        let at = |point: &Point| format!("{},{}", point.lat, point.lng);
        let mut query: Vec<(&str, String)> = vec![
            ("size", "640x360".into()),
            ("scale", "2".into()),
            ("maptype", "roadmap".into()),
            ("key", key.to_string()),
        ];
        if let Some(home) = &home_point {
            query.push(("markers", format!("icon:{HOME_ICON}|{}", at(home))));
        }
        for (index, point) in stop_points.iter().enumerate() {
            let label = if index < 9 {
                format!("label:{}|", index + 1)
            } else {
                String::new()
            };
            query.push(("markers", format!("color:0x2563eb|{label}{}", at(point))));
        }
        let path: Vec<String> = home_point.iter().chain(&stop_points).map(at).collect();
        if path.len() >= 2 {
            query.push((
                "path",
                format!("color:0x2563ebff|weight:5|{}", path.join("|")),
            ));
        }
        let pairs: Vec<(&str, &str)> = query
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let response = self
            .http
            .get(with_query(&format!("{}/staticmap", self.api_base), &pairs))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let is_image = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("image"));
        if !is_image {
            return None;
        }
        let bytes = response.bytes().await.ok()?;
        Some(format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    }

    /// Driving seconds between the home address (point 0) and the stops
    /// (points 1..n); `cost[from][to]`, infinite where there is no route.
    /// Errs with a message to show when Google fails or a stop cannot be
    /// reached from home.
    pub async fn drive_times(&self, home: &str, stops: &[String]) -> Result<Vec<Vec<f64>>, String> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| "Google Maps is not configured.".to_string())?;
        let waypoint = |address: &str| json!({ "waypoint": { "address": address } });
        let origins: Vec<Value> = std::iter::once(home)
            .chain(stops.iter().map(String::as_str))
            .map(waypoint)
            .collect();
        let destinations: Vec<Value> = stops.iter().map(|stop| waypoint(stop)).collect();
        let failed = |reason: Option<&str>| {
            reason.map_or_else(
                || "Google Maps could not build the route.".to_string(),
                |reason| format!("Google Maps could not build the route: {reason}"),
            )
        };
        let response = self
            .http
            .post(&*self.url)
            .header("X-Goog-Api-Key", &**key)
            .header(
                "X-Goog-FieldMask",
                "originIndex,destinationIndex,duration,condition",
            )
            .json(
                &json!({ "origins": origins, "destinations": destinations, "travelMode": "DRIVE" }),
            )
            .send()
            .await
            .map_err(|_| failed(None))?;
        let ok = response.status().is_success();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        let Some(elements) = body.as_array().filter(|_| ok) else {
            return Err(failed(body["error"]["message"].as_str()));
        };
        let size = stops.len() + 1;
        let mut cost = vec![vec![f64::INFINITY; size]; size];
        for (index, row) in cost.iter_mut().enumerate() {
            row[index] = 0.0;
        }
        for element in elements {
            if element["condition"] != "ROUTE_EXISTS" {
                continue;
            }
            let (Some(from), Some(to), Some(secs)) = (
                element["originIndex"].as_u64(),
                element["destinationIndex"].as_u64(),
                seconds(&element["duration"]),
            ) else {
                continue;
            };
            let (Ok(from), Ok(to)) = (usize::try_from(from), usize::try_from(to)) else {
                continue;
            };
            if from < size && to + 1 < size {
                cost[from][to + 1] = secs;
            }
        }
        let unreachable: Vec<&str> = stops
            .iter()
            .enumerate()
            .filter(|(index, _)| cost[0][index + 1].is_infinite())
            .map(|(_, stop)| stop.as_str())
            .collect();
        if unreachable.is_empty() {
            Ok(cost)
        } else {
            Err(format!(
                "Google Maps could not find a driving route to: {}",
                unreachable.join("; ")
            ))
        }
    }
}
