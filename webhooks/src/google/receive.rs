use crate::axum_helpers::guards::RemixBackend;
use crate::google::maps::get_all_address_autocompletes;
use crate::google::schemas::{AddressRequest, FinalSuggestion};
use crate::libs::constants::internal_error;
use crate::libs::types::BasicResponse;

use axum::extract;
use axum::response::Json;
use lambda_http::tracing;

pub async fn address_information(
    _: RemixBackend,
    extract::Json(payload): extract::Json<AddressRequest>,
) -> Result<Json<Vec<FinalSuggestion>>, BasicResponse> {
    match get_all_address_autocompletes(&payload.query).await {
        Ok(response) => Ok(Json(response)),
        Err(err) => {
            tracing::error!("Unable to get address autocomplete: {:?}", err);
            Err(internal_error("Unable to get address autocomplete"))
        }
    }
}
