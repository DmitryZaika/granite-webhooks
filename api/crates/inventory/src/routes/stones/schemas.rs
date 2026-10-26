use crate::openapi::{IntoParams, ToSchema};
use crate::serde_helpers::js_date;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

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
    /// `true` also returns stones hidden from the catalogs (`is_display`
    /// false). Admins only (403 otherwise). Default `false`.
    #[serde(default)]
    pub include_hidden: bool,
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
    /// Shown in catalogs. Always true unless `include_hidden` was asked for.
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

/// One stone with everything an edit form shows.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow, ToSchema)]
pub struct StoneDetail {
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
    /// Shown in the employee and customer catalogs.
    pub is_display: bool,
    /// Marked as on sale.
    pub on_sale: bool,
    /// Always orderable from the supplier, so never sold out even without slabs.
    pub regular_stock: bool,
    /// Typical slab length in inches; null if not set.
    pub length: Option<f32>,
    /// Typical slab width in inches; null if not set.
    pub width: Option<f32>,
    /// Retail price in whole dollars. 0 means the stone is sold by the slab.
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
    /// Ids of the colors the stone is tagged with, ascending.
    #[sqlx(skip)]
    pub color_ids: Vec<i32>,
}

/// A slab or remnant of a stone, as the slabs dialog lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct StoneSlab {
    /// Slab id.
    pub id: i64,
    /// Bundle label written on the slab, e.g. "B-12-1" or "Slab 2". Remnants
    /// keep the label of the slab they were cut from.
    pub bundle: Option<String>,
    /// Photo of this slab; null when it has none (show the stone image).
    pub url: Option<String>,
    /// Length in inches as entered, e.g. "126.5".
    pub length: Option<String>,
    /// Width in inches as entered, e.g. "63".
    pub width: Option<String>,
    /// The slab this remnant was cut from; null for a whole slab.
    pub parent_id: Option<i64>,
    /// Marked as a leftover piece.
    pub is_leftover: bool,
    /// Remnants cut from this slab that are not deleted (sold, free or cut).
    /// A slab with remnants can only be sold partially.
    pub child_count: i64,
    /// Set when the slab belongs to a stone this stone draws from (linked
    /// stones): that stone's id and name. Null for the stone's own slabs.
    pub source_stone: Option<StoneRef>,
    /// Id of the sale the slab is part of; null while it is available. Set
    /// even when the sale row itself is missing (then `sale` is null).
    pub sale_id: Option<i32>,
    /// The sale the slab is part of; null while it is available.
    pub sale: Option<SlabSale>,
    /// The sale of the slab this remnant was cut from; null for whole slabs
    /// and for remnants of unsold slabs.
    pub parent_sale: Option<SlabParentSale>,
}

/// A stone, by id and name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct StoneRef {
    /// Stone id.
    pub id: i32,
    /// Stone name.
    pub name: Option<String>,
}

/// The sale a slab belongs to, as shown when hovering a sold slab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SlabSale {
    /// Sale id; open it with the sale routes.
    pub id: i32,
    /// When the sale was made; null for old sales without a date.
    #[serde(serialize_with = "js_date")]
    pub sale_date: Option<DateTime<Utc>>,
    /// Customer name; null when the sale has no customer.
    pub customer_name: Option<String>,
    /// Name of the user who sold it; null when unknown.
    pub seller_name: Option<String>,
    /// Notes on the whole sale.
    pub notes: Option<String>,
    /// Total square feet of the sale, decimal string (e.g. "55.50").
    pub square_feet: Option<String>,
    /// Notes on this slab.
    pub slab_notes: Option<String>,
    /// Room this slab is used in, e.g. "Kitchen".
    pub room: Option<String>,
    /// Square feet of this slab used in the room, decimal string.
    pub slab_square_feet: Option<String>,
    /// Names of the sinks installed on any slab of this sale, one entry per
    /// sink (repeated names mean several units), in installation order.
    pub sink_names: Vec<String>,
}

/// Who bought and sold the slab a remnant was cut from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SlabParentSale {
    /// Sale id of the parent slab.
    pub id: i32,
    /// Customer name; null when the sale has no customer.
    pub customer_name: Option<String>,
    /// Seller name; null when unknown.
    pub seller_name: Option<String>,
}

/// Query string of `GET /v1/stones/search`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchStonesQuery {
    /// Part of the stone name, case-insensitive. Empty returns no stones.
    #[serde(default)]
    pub name: String,
    /// `true` also returns sold out stones. By default a stone is listed
    /// when it is regular stock or has an available whole slab.
    #[serde(default)]
    pub include_sold_out: bool,
    /// `true` also returns stones hidden from the catalog (`is_display`
    /// false). Default `false`.
    #[serde(default)]
    pub include_hidden: bool,
    /// Most stones to return, 1 to 50. Default 25.
    pub limit: Option<u32>,
}

/// Body of `POST /v1/stones/quick`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuickStoneInput {
    /// Stone name.
    pub name: String,
    /// Material type; must be one of the company's stone types.
    #[serde(rename = "type")]
    #[schema(rename = "type")]
    pub kind: String,
    /// Retail price per square foot in dollars; must be positive.
    pub retail_price: f64,
    /// Slab length in inches; 0 when unknown.
    #[serde(default)]
    pub length: Option<f64>,
    /// Slab width in inches; 0 when unknown.
    #[serde(default)]
    pub width: Option<f64>,
    /// `true` adds one leftover slab with a generated bundle (`LO-YYYYMMDD-XXXX`);
    /// `false` adds one slab per entry of `bundles`.
    pub leftover: bool,
    /// Bundle labels of the slabs to add when `leftover` is false.
    #[serde(default)]
    pub bundles: Vec<String>,
}

/// Response of `POST /v1/stones/quick`.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct QuickStone {
    /// New stone id.
    pub id: i32,
    /// Stone name.
    pub name: String,
    /// Material type.
    #[serde(rename = "type")]
    #[schema(rename = "type")]
    pub kind: String,
    /// Retail price per square foot.
    pub retail_price: f64,
    /// Ids of the slabs added, in order.
    pub slab_ids: Vec<i64>,
}
