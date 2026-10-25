//! Response of routes that create one row.

use crate::openapi::ToSchema;
use serde::Serialize;

/// The id of the row just created.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
pub struct CreatedId {
    /// Id of the new row.
    pub id: i64,
}
