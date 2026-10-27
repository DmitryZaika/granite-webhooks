//! Address suggestions from Google Places.
//!
//! - `GET /v1/places/autocomplete?q=` → US addresses matching typed text
//! - `GET /v1/places/{place_id}` → a place's address parts
//!
//! Replaces the Remix `api.google.address-complete` and
//! `api.google.place-details` routes.

use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const US_STATES: [&str; 56] = [
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA", "HI", "ID", "IL", "IN", "IA", "KS",
    "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM", "NY",
    "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA", "WV",
    "WI", "WY", "DC", "PR", "VI", "GU", "AS", "MP",
];

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(autocomplete))
        .routes(routes!(place))
}

/// Query of `GET /v1/places/autocomplete`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AutocompleteQuery {
    /// Typed text; under 3 characters gives no suggestions.
    #[serde(default)]
    pub q: String,
}

/// Address parts read from a suggestion.
#[derive(Debug, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct ParsedAddress {
    /// First part, e.g. `11378 Gammel Pl`.
    pub street: String,
    /// Second-to-last part when there are three or more.
    pub city: Option<String>,
    /// Two-letter US state.
    pub state: Option<String>,
    /// ZIP code (from the text, else looked up).
    pub zip: Option<String>,
}

/// Suggestion text.
#[derive(Debug, Serialize, ToSchema)]
pub struct SuggestionText {
    /// e.g. `11378 Gammel Pl, Fishers, IN, USA`.
    pub text: String,
}

/// An address suggestion.
#[derive(Debug, Serialize, ToSchema)]
pub struct Suggestion {
    /// Google place id.
    pub place_id: String,
    /// What Google suggests.
    pub description: SuggestionText,
    /// Its parts.
    pub address: ParsedAddress,
}

/// The last `12345` or `12345-6789` standing alone in `text` (the first
/// could be a 5-digit house number).
pub fn find_zip(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut found = None;
    let digit = |i: usize| bytes.get(i).is_some_and(u8::is_ascii_digit);
    let word = |i: usize| {
        bytes
            .get(i)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    };
    for start in 0..bytes.len() {
        if (start > 0 && word(start - 1)) || !(start..start + 5).all(digit) {
            continue;
        }
        let mut end = start + 5;
        if bytes.get(end) == Some(&b'-') && (end + 1..end + 5).all(digit) && !word(end + 5) {
            end += 5;
        } else if word(end) {
            continue;
        }
        found = Some(text[start..end].to_string());
    }
    found
}

fn without_country(text: &str) -> &str {
    let trimmed = text.trim_end();
    let lower = trimmed.to_ascii_lowercase();
    if lower.ends_with("usa") {
        trimmed[..trimmed.len() - 3]
            .trim_end()
            .trim_end_matches(',')
            .trim()
    } else {
        trimmed.trim()
    }
}

/// Street, city, state and ZIP of `12 Main St, Fishers, IN 46038, USA`.
pub fn parse_address(text: &str) -> ParsedAddress {
    let parts: Vec<&str> = without_country(text)
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let (city, state) = if parts.len() >= 3 {
        let state = parts[parts.len() - 1]
            .get(..2)
            .filter(|code| US_STATES.contains(code))
            .filter(|_| {
                parts[parts.len() - 1]
                    .as_bytes()
                    .get(2)
                    .is_none_or(|b| !b.is_ascii_alphanumeric())
            })
            .map(str::to_string);
        (Some(parts[parts.len() - 2].to_string()), state)
    } else {
        (None, None)
    };
    ParsedAddress {
        street: parts
            .first()
            .map_or_else(|| text.to_string(), |street| (*street).to_string()),
        city,
        state,
        zip: find_zip(text),
    }
}

fn component<'a>(place: &'a Value, kind: &str, text: &str) -> Option<&'a str> {
    place["addressComponents"]
        .as_array()?
        .iter()
        .find(|component| {
            component["types"]
                .as_array()
                .is_some_and(|types| types.iter().any(|t| t == kind))
        })?[text]
        .as_str()
}

/// Suggest addresses
///
/// US addresses for typed text (3 characters or more), with their parts.
/// A suggestion without a ZIP gets one from the place, else from geocoding.
#[crate::openapi::path(
    get,
    path = "/v1/places/autocomplete",
    operation_id = "autocomplete_address",
    tag = "places",
    params(AutocompleteQuery),
    responses(
        (status = 200, description = "Suggestions", body = [Suggestion]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 502, description = "Google refused", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn autocomplete(
    State(state): State<AppState>,
    _user: CurrentUser,
    Query(query): Query<AutocompleteQuery>,
) -> ApiResult<Json<Vec<Suggestion>>> {
    let q = query.q.trim();
    if q.chars().count() < 3 {
        return Ok(Json(Vec::new()));
    }
    let found = state
        .maps
        .autocomplete(q)
        .await
        .map_err(ApiError::Upstream)?;
    let lookups: Vec<_> = found
        .into_iter()
        .map(|(place_id, text)| {
            let maps = state.maps.clone();
            tokio::spawn(async move {
                let mut address = parse_address(&text);
                if address.zip.is_none() {
                    let details = maps
                        .place(&place_id, "addressComponents,formattedAddress")
                        .await;
                    address.zip = details.and_then(|place| {
                        component(&place, "postal_code", "longText")
                            .or_else(|| component(&place, "postal_code", "shortText"))
                            .map(str::to_string)
                            .or_else(|| place["formattedAddress"].as_str().and_then(find_zip))
                    });
                    if address.zip.is_none() {
                        let query = without_country(&text);
                        if !query.is_empty() {
                            address.zip = maps.geocode_zip(query).await;
                        }
                    }
                }
                Suggestion {
                    place_id,
                    description: SuggestionText { text },
                    address,
                }
            })
        })
        .collect();
    let mut suggestions = Vec::with_capacity(lookups.len());
    for lookup in lookups {
        suggestions.push(
            lookup
                .await
                .map_err(|err| ApiError::Internal(err.to_string()))?,
        );
    }
    Ok(Json(suggestions))
}

/// `GET /v1/places/{place_id}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlaceDetails {
    /// Google place id.
    pub place_id: Option<String>,
    /// Display name.
    pub name: Option<String>,
    /// Full address.
    pub formatted_address: Option<String>,
    /// ZIP code.
    pub zip_code: Option<String>,
    /// City.
    pub city: Option<String>,
    /// Two-letter state.
    pub state: Option<String>,
    /// Two-letter country.
    pub country: Option<String>,
}

/// Get a place's address
///
/// The address parts of a suggestion picked from the autocomplete.
#[crate::openapi::path(
    get,
    path = "/v1/places/{place_id}",
    operation_id = "get_place",
    tag = "places",
    params(("place_id" = String, Path, description = "Google place id")),
    responses(
        (status = 200, description = "Place", body = PlaceDetails),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 502, description = "Google refused", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn place(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(place_id): Path<String>,
) -> ApiResult<Json<PlaceDetails>> {
    let place = state
        .maps
        .place(
            &place_id,
            "id,displayName,formattedAddress,addressComponents",
        )
        .await
        .ok_or_else(|| ApiError::Upstream("Google Places Details error".into()))?;
    let text = |value: &Value| value.as_str().map(str::to_string);
    Ok(Json(PlaceDetails {
        place_id: text(&place["id"]),
        name: text(&place["displayName"]["text"]),
        formatted_address: text(&place["formattedAddress"]),
        zip_code: component(&place, "postal_code", "longText")
            .map(str::to_string)
            .or_else(|| place["formattedAddress"].as_str().and_then(find_zip)),
        city: component(&place, "locality", "longText").map(str::to_string),
        state: component(&place, "administrative_area_level_1", "shortText").map(str::to_string),
        country: component(&place, "country", "shortText").map(str::to_string),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_split_like_the_remix_parser() {
        assert_eq!(
            parse_address("11378 Gammel Pl, Fishers, IN 46038, USA"),
            ParsedAddress {
                street: "11378 Gammel Pl".into(),
                city: Some("Fishers".into()),
                state: Some("IN".into()),
                zip: Some("46038".into()),
            }
        );
        assert_eq!(
            parse_address("1 Main St, Carmel, IN, USA"),
            ParsedAddress {
                street: "1 Main St".into(),
                city: Some("Carmel".into()),
                state: Some("IN".into()),
                zip: None,
            }
        );
        assert_eq!(parse_address("Somewhere").street, "Somewhere");
    }

    #[test]
    fn zips_stand_alone() {
        assert_eq!(
            find_zip("Fishers, IN 46038-1234").as_deref(),
            Some("46038-1234")
        );
        assert_eq!(find_zip("Suite 123456, IN 46038").as_deref(), Some("46038"));
        assert_eq!(
            find_zip("11378 Gammel Pl, IN 46038").as_deref(),
            Some("46038")
        );
        assert_eq!(find_zip("no zip"), None);
    }
}
