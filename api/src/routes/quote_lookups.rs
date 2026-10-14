//! Lookups the quote editor makes while a quote is drawn.
//!
//! - `GET /v1/quotes/suggestions` → area names and miscellaneous / text items
//!   the company typed on its recent quotes, to offer them again
//! - `GET /v1/quotes/stone-image?color=&product=` → the catalog photo of one
//!   color, to paint the pieces with it in the Color & Edge preview
//!
//! Nothing is stored for the suggestions: they are read from the drawings of
//! the company's most recently changed quotes.

use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::routes::quote_editor::{StoneImage, stone_images};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(quote_suggestions))
        .routes(routes!(quote_stone_image))
}

/// How many of the company's latest quotes the suggestions are read from.
const RECENT_QUOTES: i64 = 500;
/// At most this many area names are returned.
const MAX_AREA_NAMES: usize = 60;
/// At most this many items are returned.
const MAX_ITEMS: usize = 150;
/// Longer names and descriptions are not offered again.
const MAX_TEXT: usize = 200;

/// An area name used on earlier quotes.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq)]
pub struct AreaNameSuggestion {
    /// The name, spelled as on the latest quote that used it (e.g. `Kitchen`).
    pub name: String,
    /// How many of the recent quotes use it.
    pub uses: i64,
    /// When the latest quote using it was last changed (UTC, ISO 8601); null if unknown.
    pub last_used: Option<String>,
}

/// A miscellaneous or text item used on earlier quotes, with the values it
/// had the last time.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq)]
pub struct ItemSuggestion {
    /// `misc` (a priced line typed by hand) or `text` (a line of text, no price).
    pub kind: String,
    /// The description, spelled as on the latest quote that used it.
    pub description: String,
    /// Price in dollars the last time; null when it had none (always null for `text`).
    pub price: Option<f64>,
    /// Quantity the last time (1 when not set).
    pub quantity: f64,
    /// The line was hidden on the printed quote.
    pub hide_on_quote: bool,
    /// The project discount applied to it.
    pub allow_discount: bool,
    /// Tax applied to it.
    pub taxable: bool,
    /// How many of the recent quotes use it.
    pub uses: i64,
    /// When the latest quote using it was last changed (UTC, ISO 8601); null if unknown.
    pub last_used: Option<String>,
}

/// `GET /v1/quotes/suggestions`.
#[derive(Debug, Default, Serialize, ToSchema, PartialEq)]
pub struct QuoteSuggestions {
    /// Area names, most used first (ties: most recent first). Default names
    /// such as `Area #2` are left out.
    pub area_names: Vec<AreaNameSuggestion>,
    /// Miscellaneous and text items, most used first (ties: most recent first).
    pub items: Vec<ItemSuggestion>,
}

#[derive(sqlx::FromRow)]
struct DrawingRow {
    areas: Option<String>,
    items: Option<String>,
    used_at: Option<String>,
}

/// List remembered area names and items
///
/// Area names and miscellaneous / text items (description, last price,
/// quantity and Hide On Quote / Allow Discount / Taxable flags) typed on the
/// company's 500 most recently changed, not deleted quotes, to offer them
/// again while a quote is drawn. Each comes once (names compared ignoring
/// case and spaces around them), with how many quotes used it and the values
/// of the latest one; most used first, then most recent. Default area names
/// (`Area #1`, `Area #2`, ...) are left out. At most 60 names and 150 items.
#[utoipa::path(
    get,
    path = "/v1/quotes/suggestions",
    operation_id = "list_quote_suggestions",
    tag = "quotes",
    responses(
        (status = 200, description = "Remembered names and items", body = QuoteSuggestions),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn quote_suggestions(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<QuoteSuggestions>> {
    // Only the two lists are read out of each drawing, not the whole drawing.
    let rows: Vec<DrawingRow> = sqlx::query_as(
        "SELECT CAST(JSON_EXTRACT(q.draft_drawing_json, '$.areas') AS CHAR) AS areas,
                CAST(JSON_EXTRACT(q.draft_drawing_json, '$.items') AS CHAR) AS items,
                DATE_FORMAT(COALESCE(q.updated_at, q.created_date), '%Y-%m-%dT%H:%i:%sZ') AS used_at
           FROM quotes q
          WHERE q.company_id = ? AND q.deleted_at IS NULL AND q.draft_drawing_json IS NOT NULL
          ORDER BY COALESCE(q.updated_at, q.created_date) DESC, q.id DESC
          LIMIT ?",
    )
    .bind(user.company_id)
    .bind(RECENT_QUOTES)
    .fetch_all(&state.pool)
    .await?;
    let drawings: Vec<(Value, Value, Option<String>)> = rows
        .into_iter()
        .map(|row| {
            let parse = |raw: Option<String>| {
                raw.and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .unwrap_or(Value::Null)
            };
            (parse(row.areas), parse(row.items), row.used_at)
        })
        .collect();
    Ok(Json(rank_suggestions(&drawings)))
}

/// `Area #3`, `area #12`: the names the editor gives new areas.
fn is_default_area_name(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    let Some(rest) = lower.strip_prefix("area") else {
        return false;
    };
    let rest = rest.trim_start();
    let digits = rest.strip_prefix('#').unwrap_or(rest).trim();
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// Collapses runs of spaces and trims; None when empty or too long.
fn clean(text: &str) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty() && text.chars().count() <= MAX_TEXT).then_some(text)
}

fn flag(item: &Value, key: &str, default: bool) -> bool {
    item.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// Counts names and items over drawings given newest first: one use per
/// quote, the spelling and values of the newest one, most used first.
fn rank_suggestions(drawings: &[(Value, Value, Option<String>)]) -> QuoteSuggestions {
    // key -> (order first seen in, newest first; suggestion)
    let mut order = 0usize;
    let mut names: HashMap<String, (usize, AreaNameSuggestion)> = HashMap::new();
    let mut items: HashMap<(String, String), (usize, ItemSuggestion)> = HashMap::new();
    for (areas, quote_items, used_at) in drawings {
        let mut seen_names: Vec<String> = Vec::new();
        for area in areas.as_array().into_iter().flatten() {
            let Some(name) = area.get("name").and_then(Value::as_str).and_then(clean) else {
                continue;
            };
            if is_default_area_name(&name) {
                continue;
            }
            let key = name.to_lowercase();
            if seen_names.contains(&key) {
                continue;
            }
            seen_names.push(key.clone());
            names
                .entry(key)
                .and_modify(|(_, s)| s.uses += 1)
                .or_insert_with(|| {
                    order += 1;
                    (
                        order,
                        AreaNameSuggestion {
                            name,
                            uses: 1,
                            last_used: used_at.clone(),
                        },
                    )
                });
        }
        let mut seen_items: Vec<(String, String)> = Vec::new();
        for item in quote_items.as_array().into_iter().flatten() {
            let kind = item.get("kind").and_then(Value::as_str).unwrap_or_default();
            if kind != "misc" && kind != "text" {
                continue;
            }
            let Some(description) = item.get("name").and_then(Value::as_str).and_then(clean) else {
                continue;
            };
            let key = (kind.to_string(), description.to_lowercase());
            if seen_items.contains(&key) {
                continue;
            }
            seen_items.push(key.clone());
            let misc = kind == "misc";
            items
                .entry(key)
                .and_modify(|(_, s)| s.uses += 1)
                .or_insert_with(|| {
                    order += 1;
                    (
                        order,
                        ItemSuggestion {
                            kind: kind.to_string(),
                            description,
                            price: if misc {
                                item.get("price").and_then(Value::as_f64)
                            } else {
                                None
                            },
                            quantity: item
                                .get("quantity")
                                .and_then(Value::as_f64)
                                .filter(|q| q.is_finite() && *q > 0.0)
                                .unwrap_or(1.0),
                            hide_on_quote: flag(item, "hideOnQuote", false),
                            allow_discount: flag(item, "allowDiscount", true),
                            taxable: flag(item, "taxable", true),
                            uses: 1,
                            last_used: used_at.clone(),
                        },
                    )
                });
        }
    }
    let mut names: Vec<(usize, AreaNameSuggestion)> = names.into_values().collect();
    names.sort_by(|a, b| b.1.uses.cmp(&a.1.uses).then(a.0.cmp(&b.0)));
    let mut items: Vec<(usize, ItemSuggestion)> = items.into_values().collect();
    items.sort_by(|a, b| b.1.uses.cmp(&a.1.uses).then(a.0.cmp(&b.0)));
    QuoteSuggestions {
        area_names: names
            .into_iter()
            .take(MAX_AREA_NAMES)
            .map(|(_, s)| s)
            .collect(),
        items: items.into_iter().take(MAX_ITEMS).map(|(_, s)| s).collect(),
    }
}

/// Query of `GET /v1/quotes/stone-image`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct StoneImageQuery {
    /// Color name as picked on the quote (any case), e.g. `Calacatta Laza`.
    #[serde(default)]
    pub color: String,
    /// Product (material) it was picked under (any case); a stone of this
    /// type is preferred, any type is used otherwise. May be empty.
    #[serde(default)]
    pub product: String,
}

/// `GET /v1/quotes/stone-image`.
#[derive(Debug, Serialize, ToSchema)]
pub struct StoneImageLookup {
    /// The color's photo; null when the company's catalog has no stone of
    /// that name with a photo (e.g. an Other Color typed by hand).
    pub image: Option<StoneImage>,
}

/// Get a color's stone photo
///
/// The catalog photo of one color as the quote shows it on the customer's
/// preview: the company's in-stock stone with this name (regular stock, or a
/// slab not sold, cut or deleted; a stone of the given product type first,
/// one with free slabs first). Returns `{"image": null}` when no stone of
/// that name is in stock or `color` is empty. The editor uses
/// it to paint the area's pieces in the Color & Edge preview.
#[utoipa::path(
    get,
    path = "/v1/quotes/stone-image",
    operation_id = "get_quote_stone_image",
    tag = "quotes",
    params(StoneImageQuery),
    responses(
        (status = 200, description = "The photo, or null", body = StoneImageLookup),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn quote_stone_image(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<StoneImageQuery>,
) -> ApiResult<Json<StoneImageLookup>> {
    let color = query.color.trim();
    if color.is_empty() {
        return Ok(Json(StoneImageLookup { image: None }));
    }
    // The same lookup as the quote's previews, for a drawing with this one pick.
    let doc = json!({
        "areas": [{ "id": "a" }],
        "colorOptions": [{ "byArea": { "a": { "color": color, "product": query.product.trim() } } }],
    });
    let image = stone_images(&state.pool, user.company_id, &doc, true)
        .await?
        .into_iter()
        .next();
    Ok(Json(StoneImageLookup { image }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_area_names_are_skipped() {
        assert!(is_default_area_name("Area #1"));
        assert!(is_default_area_name(" area #12 "));
        assert!(is_default_area_name("Area 3"));
        assert!(!is_default_area_name("Area"));
        assert!(!is_default_area_name("Kitchen"));
        assert!(!is_default_area_name("Area #1 island"));
    }

    #[test]
    fn ranks_by_uses_then_recency_with_latest_values() {
        let drawings = vec![
            (
                json!([{ "name": "kitchen " }, { "name": "Area #2" }, { "name": "Bar" }]),
                json!([
                    { "kind": "misc", "name": "Haul away", "price": 150.0, "taxable": false },
                    { "kind": "priceList", "name": "Sink cutout" },
                    { "kind": "text", "name": "Customer supplies sink", "price": 5 },
                ]),
                Some("2026-10-10T00:00:00Z".to_string()),
            ),
            (
                json!([{ "name": "Kitchen" }, { "name": "Island" }, { "name": "KITCHEN" }]),
                json!([{ "kind": "misc", "name": "haul away", "price": 100.0 }]),
                Some("2026-10-01T00:00:00Z".to_string()),
            ),
        ];
        let out = rank_suggestions(&drawings);
        let names: Vec<&str> = out.area_names.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["kitchen", "Bar", "Island"]);
        assert_eq!(out.area_names[0].uses, 2);
        assert_eq!(out.items.len(), 2);
        let haul = &out.items[0];
        assert_eq!(haul.description, "Haul away");
        assert_eq!(haul.uses, 2);
        assert_eq!(haul.price, Some(150.0));
        assert!(!haul.taxable);
        assert!(haul.allow_discount);
        let text = &out.items[1];
        assert_eq!(text.kind, "text");
        assert_eq!(text.price, None);
    }
}
