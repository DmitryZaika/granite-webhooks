use serde::Serialize;
use sqlx::FromRow;
use utoipa::ToSchema;

/// A calendar of the company's schedule.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct Calendar {
    /// Calendar id; events reference it.
    pub id: i32,
    /// Display name.
    pub name: String,
    /// Color name used on the schedule (e.g. `blue`).
    pub color: String,
    /// `templates`, `installations` or `estimates` for the built-in
    /// calendars every company has; null for calendars the company added.
    pub slug: Option<String>,
    /// A built-in calendar (it cannot be renamed or deleted).
    pub is_system: bool,
}
