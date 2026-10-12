use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;

/// A supplier the company buys from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct Supplier {
    /// Supplier id.
    pub id: i32,
    /// Supplier name.
    pub name: String,
}

/// A stone color tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct Color {
    /// Color id.
    pub id: i32,
    /// Color name, e.g. "Black".
    pub name: String,
    /// Display color, `#RRGGBB`.
    pub hex_code: String,
}

/// A stone type (material) of the company, e.g. "granite".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct StoneType {
    /// Stone type id.
    pub id: i64,
    /// Type name as entered; stones store it in their `type`.
    pub name: String,
}

/// Body of `POST /v1/stone-types` and `PATCH /v1/stone-types/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StoneTypeInput {
    /// 1 to 255 characters; unique per company, ignoring case.
    pub name: String,
}

/// 409 body of `DELETE /v1/stone-types/{id}`: the type is still used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct StoneTypeInUse {
    /// "This type is used by undeleted stones and cannot be deleted."
    pub error: String,
    /// Live stones of that type, by name; delete or retype them first.
    pub stones: Vec<StoneUsingType>,
}

/// 409 body of `DELETE /v1/sink-types/{id}`: the type is still used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SinkTypeInUse {
    /// "This type is used by undeleted sinks and cannot be deleted."
    pub error: String,
    /// Live sink models of that type, by name; delete or retype them first.
    pub sinks: Vec<StoneUsingType>,
}

/// A stone (or sink model) that uses a type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct StoneUsingType {
    /// Stone id.
    pub id: i32,
    /// Stone name.
    pub name: Option<String>,
}
