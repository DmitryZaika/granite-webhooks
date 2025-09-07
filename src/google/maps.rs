use reqwest::Client;
use serde::Deserialize;

/// Ошибка высокого уровня
#[derive(thiserror::Error, Debug)]
pub enum DistanceError {
    #[error("Google API error: {0}")]
    Api(String),
    #[error("Element status: {0}")]
    ElementStatus(String),
    #[error("Network: {0}")]
    Net(#[from] reqwest::Error),
    #[error("Unexpected response shape")]
    Shape,
}

#[derive(Deserialize, Debug)]
struct TextValue {
    // значение всегда в метрах
    value: u64,
    // человекочитаемый текст (мили/км зависят от параметра units)
    text: String,
}

#[derive(Deserialize, Debug)]
struct Element {
    status: String,
    distance: Option<TextValue>,
}

#[derive(Deserialize, Debug)]
struct Row {
    elements: Vec<Element>,
}

#[derive(Deserialize, Debug)]
struct DmResponse {
    status: String,
    error_message: Option<String>,
    rows: Option<Vec<Row>>,
}

/// Возвращает расстояние по дороге в милях между origin и destination.
/// По умолчанию — режим "driving".
pub async fn driving_distance_miles(
    origin: &str,
    destination: &str,
    api_key: &str,
) -> Result<f64, DistanceError> {
    let client = Client::new();
    let url = "https://maps.googleapis.com/maps/api/distancematrix/json";

    // units=imperial влияет только на текст, value всегда в метрах
    let resp = client
        .get(url)
        .query(&[
            ("origins", origin),
            ("destinations", destination),
            ("mode", "driving"),
            ("units", "imperial"),
            ("key", api_key),
        ])
        .send()
        .await?
        .error_for_status()? // HTTP != 2xx -> ошибка
        .json::<DmResponse>()
        .await?;

    if resp.status != "OK" {
        return Err(DistanceError::Api(
            resp.error_message.unwrap_or_else(|| resp.status),
        ));
    }

    let rows = resp.rows.ok_or(DistanceError::Shape)?;
    let first_el = rows
        .get(0)
        .and_then(|r| r.elements.get(0))
        .ok_or(DistanceError::Shape)?;

    if first_el.status != "OK" {
        return Err(DistanceError::ElementStatus(first_el.status.clone()));
    }

    let meters = first_el
        .distance
        .as_ref()
        .ok_or(DistanceError::Shape)?
        .value as f64;

    // 1 mile = 1609.344 meters
    Ok(meters / 1609.344_f64)
}
