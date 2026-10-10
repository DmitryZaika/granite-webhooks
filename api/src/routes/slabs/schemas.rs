use serde::Deserialize;
use utoipa::ToSchema;

/// Body of `PATCH /v1/slabs/{id}`.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateSlab {
    /// New length in inches; a positive number, decimals allowed.
    pub length: f64,
    /// New width in inches; a positive number, decimals allowed.
    pub width: f64,
}

/// Query string of `GET /v1/slabs`.
#[derive(Debug, Default, serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListSlabsQuery {
    /// Slab ids to return; repeat the parameter (`?id=1&id=2`). Required:
    /// without ids the list is empty.
    #[serde(default, rename = "id")]
    #[param(rename = "id")]
    pub ids: Vec<i64>,
}

/// A slab by id, with the stone it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow, ToSchema)]
pub struct SlabSummary {
    /// Slab id.
    pub id: i64,
    /// Bundle label, e.g. "B-12-1".
    pub bundle: Option<String>,
    /// Marked as a leftover piece.
    pub is_leftover: bool,
    /// The slab's stone.
    #[sqlx(flatten)]
    pub stone: SlabStone,
}

/// The stone a slab belongs to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow, ToSchema)]
pub struct SlabStone {
    /// Stone id.
    #[sqlx(rename = "stone_id")]
    pub id: i32,
    /// Stone name.
    #[sqlx(rename = "stone_name")]
    pub name: Option<String>,
    /// Material type, e.g. `granite`.
    #[sqlx(rename = "stone_type")]
    #[serde(rename = "type")]
    pub kind: Option<String>,
}
