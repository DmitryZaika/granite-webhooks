use crate::openapi::ToSchema;
use serde::Serialize;
use sqlx::FromRow;

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
    /// Hidden from everyone's schedule (only `all=true` lists these).
    pub is_hidden: bool,
    /// The day's route map and route buttons use its events (on by default
    /// for Installations and Templates).
    pub shows_day_map: bool,
}

/// A calendar an admin hid from one user.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct HiddenUser {
    /// The calendar.
    pub calendar_id: i32,
    /// The user it is hidden from.
    pub user_id: i32,
}
