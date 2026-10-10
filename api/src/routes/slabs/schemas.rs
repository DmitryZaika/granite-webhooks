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
