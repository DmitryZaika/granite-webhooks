fn get_distance(origin: &str, destination: &str) -> Result<f64, Error> {
    let url = format!("https://maps.googleapis.com/maps/api/distancematrix/json?origins={}&destinations={}&key={}", origin, destination, key);
    let response = reqwest::get(url).await?;
    let body = response.text().await?;
    let data: serde_json::Value = serde_json::from_str(&body)?;
    let distance = data["rows"][0]["elements"][0]["distance"]["value"].as_f64().unwrap();
    Ok(distance)
}