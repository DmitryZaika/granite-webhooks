use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::{IntoParams, ToSchema};

/// Query string of `GET /v1/faucets`. Every filter is optional.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListFaucetsQuery {
    /// Only faucet models of these types: `single handle`, `double handle`.
    /// Repeat the parameter for several.
    #[serde(default, rename = "type")]
    #[param(rename = "type")]
    pub types: Vec<String>,
    /// Only faucet models bought from this supplier (a supplier id).
    pub supplier_id: Option<i32>,
    /// `true` also returns sold out models: no free unit and not regular
    /// stock. Default `false`.
    #[serde(default)]
    pub include_sold_out: bool,
}

/// A faucet model the company sells, with how many units are free to sell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct Faucet {
    /// Faucet model id.
    pub id: i32,
    /// Model name, e.g. "Pull-Down Chrome".
    pub name: String,
    /// Faucet type, e.g. `single handle` or `double handle`.
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
    /// Retail price as a decimal string, e.g. `"129.99"`; null if not set.
    pub retail_price: Option<String>,
    /// Purchase cost as a decimal string; null if not set.
    pub cost: Option<String>,
    /// Units in stock that are not installed on a sold slab.
    pub available: i64,
}
