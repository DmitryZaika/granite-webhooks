use crate::serde_helpers::js_date;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::FromRow;
use utoipa::ToSchema;

/// A post-installation checklist: what the installer checked on site after
/// installing countertops, signed by the customer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct Checklist {
    /// Checklist id. Its PDF is served by the web app at `/api/checklist-pdf/{id}`.
    pub id: i32,
    /// Customer record the checklist belongs to; null when the installer
    /// typed a name without picking a customer, or the customer was deleted.
    pub customer_id: Option<i32>,
    /// Customer name as written on the checklist.
    pub customer_name: String,
    /// Address of the installation.
    pub installation_address: String,
    /// Customer email the satisfaction survey goes to; null if not given.
    pub email: Option<String>,
    /// User id of the installer who filled it in; null if unknown.
    pub installer_id: Option<i32>,
    /// Installer's name, also for installers who were deleted since; null
    /// when there is no installer.
    pub installer_name: Option<String>,
    /// Each item of the checklist, `true` when it passed.
    #[sqlx(flatten)]
    pub checks: ChecklistChecks,
    /// Installer's or customer's comments; null or empty when none.
    pub comments: Option<String>,
    /// When the checklist was submitted (ISO 8601, UTC).
    #[serde(serialize_with = "js_date")]
    pub created_at: Option<DateTime<Utc>>,
}

/// The items the customer confirms with the installer (labels as on the
/// form); `true` means it passed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct ChecklistChecks {
    /// "Material is correct".
    pub material_correct: bool,
    /// "Seams meet my satisfaction" (the customer's).
    pub seams_satisfaction: bool,
    /// "Appliances fit properly".
    pub appliances_fit: bool,
    /// "Backsplashes placed correctly".
    pub backsplashes_correct: bool,
    /// "Edges and corners are correct".
    pub edges_correct: bool,
    /// "Holes for fixtures are drilled".
    pub holes_drilled: bool,
    /// "Clean up completed".
    pub cleanup_completed: bool,
}
