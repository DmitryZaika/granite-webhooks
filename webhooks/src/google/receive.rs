use crate::axum_helpers::guards::RemixBackend;
use crate::crud::company::{
    get_company_address, get_company_coordinates, update_company_coordinates,
};
use crate::google::maps::{geocode_address, get_all_address_autocompletes};
use crate::google::schemas::{AddressRequest, FinalSuggestion, LatLng};
use crate::libs::constants::internal_error;
use crate::libs::types::BasicResponse;

use axum::extract::{self, Path, State};
use axum::response::Json;
use lambda_http::tracing;
use reqwest::StatusCode;
use sqlx::MySqlPool;

pub async fn address_information(
    _: RemixBackend,
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    extract::Json(payload): extract::Json<AddressRequest>,
) -> Result<Json<Vec<FinalSuggestion>>, BasicResponse> {
    let bias_coords = match get_company_coordinates(&pool, company_id).await {
        Ok(Some(coords)) => Some(LatLng {
            latitude: coords.latitude,
            longitude: coords.longitude,
        }),
        Ok(None) => None,
        Err(err) => {
            tracing::error!("Failed to fetch company coordinates: {:?}", err);
            None
        }
    };

    match get_all_address_autocompletes(&payload.query, bias_coords).await {
        Ok(response) => Ok(Json(response)),
        Err(err) => {
            tracing::error!("Unable to get address autocomplete: {:?}", err);
            Err(internal_error("Unable to get address autocomplete"))
        }
    }
}

/// Fills in the company's latitude and longitude by geocoding its stored address,
/// but only if coordinates are not already set.
pub async fn fill_company_coordinates(
    _: RemixBackend,
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
) -> BasicResponse {
    // Skip if coordinates already exist
    match get_company_coordinates(&pool, company_id).await {
        Ok(Some(_)) => {
            tracing::info!(
                company_id = company_id,
                "Company already has coordinates, skipping"
            );
            return crate::libs::constants::OK_RESPONSE;
        }
        Ok(None) => {} // continue to geocode
        Err(err) => {
            tracing::error!("Failed to check company coordinates: {:?}", err);
            return internal_error("Failed to check company coordinates");
        }
    }

    // Get the company's address
    let address = match get_company_address(&pool, company_id).await {
        Ok(Some(addr)) => addr,
        Ok(None) => {
            tracing::error!(company_id = company_id, "Company has no address set");
            return (StatusCode::NOT_FOUND, "Company address not found");
        }
        Err(err) => {
            tracing::error!("Failed to fetch company address: {:?}", err);
            return internal_error("Failed to fetch company address");
        }
    };

    // Geocode the address
    let coords = match geocode_address(&address).await {
        Ok(coords) => coords,
        Err(err) => {
            tracing::error!(
                company_id = company_id,
                address = %address,
                "Failed to geocode company address: {:?}", err
            );
            return internal_error("Failed to geocode company address");
        }
    };

    // Save the coordinates
    if let Err(err) =
        update_company_coordinates(&pool, company_id, coords.latitude, coords.longitude).await
    {
        tracing::error!("Failed to save company coordinates: {:?}", err);
        return internal_error("Failed to save company coordinates");
    }

    tracing::info!(
        company_id = company_id,
        latitude = coords.latitude,
        longitude = coords.longitude,
        "Successfully filled company coordinates"
    );

    crate::libs::constants::OK_RESPONSE
}
