use crate::openapi::{IntoParams, ToSchema};
use crate::routes::stones::schemas::SlabCounts;
use crate::serde_helpers::js_date;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// A company of the showroom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct PublicCompany {
    /// Company id; use it in the other public routes.
    pub id: i32,
}

/// Query of `GET /v1/public/companies`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PublicCompaniesQuery {
    /// Company ids (repeat the parameter); at most 20.
    #[serde(default)]
    pub ids: Vec<i32>,
}

/// A company's name and logo for a showroom chooser.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct PublicCompanyCard {
    /// Company id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Logo URL; null when it has none.
    pub logo_url: Option<String>,
}

/// Filters of `GET /v1/public/companies/{company_id}/stones`; list filters
/// take the parameter repeated and match any value.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PublicStonesQuery {
    /// Only these types, e.g. `granite`, `quartz`.
    #[serde(default, rename = "type")]
    #[param(rename = "type")]
    pub types: Vec<String>,
    /// Only stones tagged with at least one of these colors (color ids).
    #[serde(default, rename = "color_id")]
    #[param(rename = "color_id")]
    pub color_ids: Vec<i32>,
    /// Only these price levels (1 = cheapest ... 7).
    #[serde(default)]
    pub level: Vec<i32>,
    /// Only these finishes: `polished`, `leathered`, `honed`.
    #[serde(default)]
    pub finishing: Vec<String>,
}

/// Query string of `GET /v1/public/companies/{company_id}/stones/search`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PublicSearchQuery {
    /// Part of the stone name, case-insensitive; empty returns nothing.
    #[serde(default)]
    pub name: String,
    /// Most stones to return, 1 to 50. Default 25.
    pub limit: Option<u32>,
}

/// A stone of the showroom: no costs, only what customers may see.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow, ToSchema)]
pub struct PublicStone {
    /// Stone id.
    pub id: i32,
    /// Stone name.
    pub name: Option<String>,
    /// Material type, e.g. `granite`.
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Image URL; null when none. Thumbnails are at `<name>-icon.<ext>`.
    pub url: Option<String>,
    /// Typical slab length in inches.
    pub length: Option<f32>,
    /// Typical slab width in inches.
    pub width: Option<f32>,
    /// Marked as on sale.
    pub on_sale: bool,
    /// Always orderable.
    pub regular_stock: bool,
    /// Retail price per square foot in dollars; 0 means priced by slab
    /// (see `slab_price_per_sqft`).
    pub retail_price: Option<i32>,
    /// Price per square foot of a slab, set only for stones priced by slab
    /// (`retail_price` 0); null otherwise.
    pub slab_price_per_sqft: Option<i32>,
    /// Price level 1 (cheapest) to 7; contractor discounts depend on it.
    pub level: Option<i32>,
    /// Surface finish.
    pub finishing: Option<String>,
    /// Supplier bundle number.
    pub bundle_number: Option<String>,
    /// When the stone was added.
    #[serde(serialize_with = "js_date")]
    pub created_date: Option<DateTime<Utc>>,
    /// Slabs in the yard (own and linked stones).
    #[sqlx(flatten)]
    pub slabs: SlabCounts,
}

/// An available slab of a showroom stone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct PublicSlab {
    /// Slab id.
    pub id: i64,
    /// Bundle label.
    pub bundle: Option<String>,
    /// Photo of the slab; null when none (show the stone image).
    pub url: Option<String>,
    /// Length in inches as entered.
    pub length: Option<String>,
    /// Width in inches as entered.
    pub width: Option<String>,
}

/// A photo of a stone installed in a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct PublicPhoto {
    /// Photo id.
    pub id: i32,
    /// Photo URL.
    pub url: Option<String>,
}

/// Filter of the public sink and faucet lists.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PublicFixturesQuery {
    /// Only these types; repeat the parameter for several.
    #[serde(default, rename = "type")]
    #[param(rename = "type")]
    pub types: Vec<String>,
}

/// A sink or faucet model of the showroom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct PublicFixture {
    /// Model id.
    pub id: i32,
    /// Model name.
    pub name: String,
    /// Model type.
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    pub kind: String,
    /// Image URL; may be empty.
    pub url: String,
    /// Length in inches (sinks); null for faucets.
    pub length: Option<i32>,
    /// Width in inches (sinks); null for faucets.
    pub width: Option<i32>,
    /// Always orderable.
    pub regular_stock: bool,
    /// Units free to sell.
    pub available: i64,
    /// Retail price, decimal string; null if not set.
    pub retail_price: Option<String>,
}
