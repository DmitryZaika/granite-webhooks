use crate::serde_helpers::js_date;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::{IntoParams, ToSchema};

/// Query string of `GET /v1/stones`. Every filter is optional; list filters
/// take the parameter repeated (`?type=granite&type=quartz`) and match any of
/// the given values.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListStonesQuery {
    /// Only stones of these types, e.g. `granite`, `quartz`, `marble`.
    #[serde(default, rename = "type")]
    #[param(rename = "type")]
    pub types: Vec<String>,
    /// Only stones bought from this supplier (a supplier id).
    pub supplier_id: Option<i32>,
    /// Only stones tagged with at least one of these colors (color ids).
    #[serde(default, rename = "color_id")]
    #[param(rename = "color_id")]
    pub color_ids: Vec<i32>,
    /// Only stones of these price levels (1 = cheapest ... 7).
    #[serde(default)]
    pub level: Vec<i32>,
    /// Only stones with these surface finishes: `polished`, `leathered`, `honed`.
    #[serde(default)]
    pub finishing: Vec<String>,
    /// `true` also returns sold out stones: no available slab and not regular
    /// stock. Default `false`.
    #[serde(default)]
    pub include_sold_out: bool,
}

/// A stone (a color/material the company sells) with its slab inventory.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow, ToSchema)]
pub struct Stone {
    /// Stone id.
    pub id: i32,
    /// Display name, e.g. "Absolute Black".
    pub name: Option<String>,
    /// Material type, lowercase: `granite`, `quartz`, `marble`, `quartzite`...
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Image URL; null when the stone has no picture.
    pub url: Option<String>,
    /// Supplier id; null if unknown.
    pub supplier_id: Option<i32>,
    /// Shown in catalogs. This route only returns displayed stones, so always true.
    pub is_display: bool,
    /// Marked as on sale.
    pub on_sale: bool,
    /// Always orderable from the supplier, so never sold out even without slabs.
    pub regular_stock: bool,
    /// Typical slab length in inches; null if not set.
    pub length: Option<f32>,
    /// Typical slab width in inches; null if not set.
    pub width: Option<f32>,
    /// Retail price in whole dollars. 0 means the stone is sold by the slab
    /// at `cost_per_sqft`.
    pub retail_price: Option<i32>,
    /// Cost per square foot in whole dollars.
    pub cost_per_sqft: Option<i32>,
    /// Price level, 1 (cheapest) to 7; null if not set.
    pub level: Option<i32>,
    /// Surface finish: `polished`, `leathered` or `honed`; null if not set.
    pub finishing: Option<String>,
    /// Number of samples on hand.
    pub samples_amount: Option<i32>,
    /// How important it is to keep samples in stock; null if not set.
    pub samples_importance: Option<i32>,
    /// Supplier bundle number, free text.
    pub bundle_number: Option<String>,
    /// Where the bundle is stored in the yard, free text.
    pub bundle_location: Option<String>,
    /// Expected delivery date, `YYYY-MM-DD`; null if not set.
    pub delivery_date: Option<String>,
    /// When the stone was added.
    #[serde(serialize_with = "js_date")]
    pub created_date: Option<DateTime<Utc>>,
    /// Slabs on hand.
    #[sqlx(flatten)]
    pub slabs: SlabCounts,
}

/// Slab counts of a stone. Counts the stone's own slabs plus those of the
/// stones it draws from (linked stones). Cut and deleted slabs are never
/// counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct SlabCounts {
    /// Slabs and remnants in the yard, sold or not.
    #[sqlx(rename = "slabs_total")]
    pub total: i64,
    /// Slabs and remnants not tied to a sale.
    #[sqlx(rename = "slabs_available")]
    pub available: i64,
    /// Whole slabs (not remnants) in the yard, sold or not.
    #[sqlx(rename = "slabs_whole_total")]
    pub whole_total: i64,
    /// Whole slabs (not remnants) not tied to a sale.
    #[sqlx(rename = "slabs_whole_available")]
    pub whole_available: i64,
}
