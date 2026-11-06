//! Where an address is, so the app's directions start driving at once.

use crate::auth::device::DeviceUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::maps::Point;
use crate::openapi::IntoParams;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Deserialize;

/// Longest address looked up.
const MAX_ADDRESS_CHARS: usize = 300;

/// Query of `GET /v1/phone/directions`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DirectionsQuery {
    /// The address to drive to, as the app shows it (an event's or a
    /// customer's), e.g. `6309 Looking Glass Lane, Indianapolis, IN 46235`.
    /// At most 300 characters.
    pub address: String,
}

/// Find where an address is, for driving directions
///
/// Returns the address's latitude and longitude (Google geocoding), which
/// the app hands to the phone's maps app so turn-by-turn navigation starts
/// at once: a map app given only the words may show the place, or a list
/// of matches, first. 404 when Google can't find the address (the app then
/// passes the words as before); 400 for an empty or too long address.
/// Device token only.
#[crate::openapi::path(
    get,
    path = "/v1/phone/directions",
    operation_id = "find_phone_directions_point",
    tag = "phone",
    params(DirectionsQuery),
    responses(
        (status = 200, description = "Where the address is", body = Point),
        (status = 400, description = "Empty or too long address", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 404, description = "Google can't find the address", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn find_directions_point(
    State(state): State<AppState>,
    _device: DeviceUser,
    MultiQuery(query): MultiQuery<DirectionsQuery>,
) -> ApiResult<Json<Point>> {
    let address = query.address.trim();
    if address.is_empty() || address.chars().count() > MAX_ADDRESS_CHARS {
        return Err(ApiError::BadRequest("Enter an address".into()));
    }
    state
        .maps
        .geocode(address)
        .await
        .map(Json)
        .ok_or(ApiError::NotFound)
}
