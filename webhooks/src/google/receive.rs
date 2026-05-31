use crate::axum_helpers::guards::RemixBackend;
use crate::google::maps::get_address_autocomplete;
use crate::google::schemas::{AddressRequest, FinalSuggestion};
use crate::libs::constants::{NOT_FOUND_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;

use axum::extract;
use axum::response::Json;
use lambda_http::tracing;

pub async fn address_information(
    _: RemixBackend,
    extract::Json(payload): extract::Json<AddressRequest>,
) -> Result<Json<FinalSuggestion>, BasicResponse> {
    match get_address_autocomplete(&payload.query).await {
        Ok(Some(response)) => Ok(Json(response)),
        Ok(None) => Err(NOT_FOUND_RESPONSE),
        Err(err) => {
            tracing::error!("Unable to get address autocomplete: {:?}", err);
            Err(internal_error("Unable to get address autocomplete"))
        }
    }
}
