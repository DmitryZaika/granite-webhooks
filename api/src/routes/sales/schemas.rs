use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

/// A sale as the sell / edit form shows it: who bought, who sold, and the
/// rooms with their slabs, sinks and faucets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SaleContract {
    /// Sale id.
    pub id: i32,
    /// Customer who bought.
    pub customer_id: i32,
    /// User credited with the sale; null if unknown.
    pub seller_id: Option<i32>,
    /// Address of the job; null or empty means the customer's address.
    pub project_address: Option<String>,
    /// Notes on the whole sale.
    pub notes: Option<String>,
    /// Total price, decimal string (e.g. "4200.00"); null if not set.
    pub price: Option<String>,
    /// The customer's company name; null for private customers.
    pub company_name: Option<String>,
    /// Extra line items of the whole sale (`[{"adjustment": "...", "price": 0}]`),
    /// as saved; null when none were saved.
    #[schema(value_type = Option<Object>)]
    pub extras: Option<Value>,
    /// Rooms in the order their first slab was added.
    pub rooms: Vec<SaleRoom>,
}

/// One room of a sale. Room fields are stored on each of its slabs; these
/// come from the room's first slab. Nulls mean "not set" (the form uses its
/// defaults).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SaleRoom {
    /// Room UUID shared by its slabs; null for old sales saved without one.
    pub room_id: Option<String>,
    /// Room name, e.g. `kitchen`, `bathroom`, or free text.
    pub room: Option<String>,
    /// Backsplash: `no`, `4 inch`, `full height`, or free text.
    pub backsplash: Option<String>,
    /// Square feet of the room, decimal string.
    pub square_feet: Option<String>,
    /// Price per square foot, decimal string.
    pub retail_price: Option<String>,
    /// Tear-out option.
    pub tear_out: Option<String>,
    /// Stove option.
    pub stove: Option<String>,
    /// Waterfall option.
    pub waterfall: Option<String>,
    /// Number of corbels.
    pub corbels: Option<i32>,
    /// Seam option.
    pub seam: Option<String>,
    /// Ten year sealer included.
    pub ten_year_sealer: bool,
    /// Room extras (edge, tear-out, stove... prices) as saved.
    #[schema(value_type = Option<Object>)]
    pub extras: Option<Value>,
    /// Slabs used in the room.
    pub slabs: Vec<SaleSlab>,
    /// Sinks installed on the room's first slab.
    pub sinks: Vec<SaleFixture>,
    /// Faucets installed on the room's first slab.
    pub faucets: Vec<SaleFixture>,
}

/// A slab used in a room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SaleSlab {
    /// Slab id.
    pub id: i64,
    /// `true` when the whole slab is sold. `false` when only part of it is:
    /// selling then leaves a remnant (a child slab) in the yard.
    pub is_full: bool,
}

/// A sink or faucet unit on a sale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SaleFixture {
    /// Unit id (`sinks.id` / `faucets.id`).
    pub id: i32,
    /// Model id (`sink_type.id` / `faucet_type.id`).
    pub type_id: i32,
    /// Price charged for the unit, decimal string; null if not set.
    pub price: Option<String>,
}

/// Body of `POST /v1/sales` and `PUT /v1/sales/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SaleInput {
    /// Customer who buys; must be a live customer of the company.
    pub customer_id: i32,
    /// User credited with the sale. Defaults to the caller when selling;
    /// left unchanged when editing.
    #[serde(default)]
    pub seller_id: Option<i32>,
    /// Address of the job. Empty or missing uses the customer's address.
    #[serde(default)]
    pub project_address: Option<String>,
    /// Notes on the whole sale; empty clears them.
    #[serde(default)]
    pub notes: Option<String>,
    /// Total price in dollars.
    #[serde(default)]
    pub price: f64,
    /// Extra line items of the whole sale, stored as given.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub extras: Value,
    /// At least one room, each with at least one slab.
    pub rooms: Vec<RoomInput>,
}

/// One room of a sale being saved.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomInput {
    /// Room UUID (any version), shared by its slabs.
    pub room_id: String,
    /// Room name, e.g. `kitchen`.
    pub room: String,
    /// Backsplash option.
    pub backsplash: String,
    /// Square feet of the room; must be positive.
    pub square_feet: Option<f64>,
    /// Price per square foot in dollars.
    #[serde(default)]
    pub retail_price: f64,
    /// Tear-out option.
    pub tear_out: String,
    /// Stove option.
    pub stove: String,
    /// Waterfall option.
    pub waterfall: String,
    /// Number of corbels.
    #[serde(default)]
    pub corbels: i32,
    /// Seam option.
    pub seam: String,
    /// Ten year sealer included.
    #[serde(default)]
    pub ten_year_sealer: bool,
    /// Room extras, stored as given.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub extras: Value,
    /// Slabs used in the room. The same slab may appear in several rooms:
    /// each later room gets its own piece cut from it.
    pub slabs: Vec<SaleSlab>,
    /// Sinks to install: one free unit of each model is taken.
    #[serde(default)]
    pub sinks: Vec<FixtureInput>,
    /// Faucets to install: one free unit of each model is taken.
    #[serde(default)]
    pub faucets: Vec<FixtureInput>,
}

/// A sink or faucet model to install, with the price charged.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FixtureInput {
    /// Model id (`sink_type.id` / `faucet_type.id`).
    pub type_id: i32,
    /// Price charged in dollars.
    #[serde(default)]
    pub price: f64,
}

/// Response of `POST /v1/sales`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct CreatedSale {
    /// Id of the new sale.
    pub id: i32,
}
