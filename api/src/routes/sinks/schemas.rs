use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::{IntoParams, ToSchema};

/// Query string of `GET /v1/sinks`. Every filter is optional.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListSinksQuery {
    /// Only sink models of these types: `stainless 18 gauge`, `stainless 16 gauge`,
    /// `composite`, `ceramic`, `farm house`. Repeat the parameter for several.
    #[serde(default, rename = "type")]
    #[param(rename = "type")]
    pub types: Vec<String>,
    /// Only sink models bought from this supplier (a supplier id).
    pub supplier_id: Option<i32>,
    /// `true` also returns sold out models: no free unit and not regular
    /// stock. Default `false`.
    #[serde(default)]
    pub include_sold_out: bool,
}

/// A sink model the company sells, with how many units are free to sell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct Sink {
    /// Sink model id.
    pub id: i32,
    /// Model name, e.g. "Undermount 3219".
    pub name: String,
    /// Sink type, e.g. `stainless 18 gauge` or `farm house`.
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    pub kind: String,
    /// Image URL; may be empty.
    pub url: String,
    /// Supplier id; null if unknown.
    pub supplier_id: Option<i32>,
    /// Shown to customers in the showroom catalog.
    pub is_display: bool,
    /// Always orderable from the supplier, so never sold out even with no units.
    pub regular_stock: bool,
    /// Length in inches; 0 if not set.
    pub length: i32,
    /// Width in inches; 0 if not set.
    pub width: i32,
    /// Retail price as a decimal string, e.g. `"199.00"`; null if not set.
    pub retail_price: Option<String>,
    /// Purchase cost as a decimal string; null if not set.
    pub cost: Option<String>,
    /// Units in stock that are not installed on a sold slab.
    pub available: i64,
}
