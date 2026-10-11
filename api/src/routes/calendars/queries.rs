use super::schemas::Calendar;
use sqlx::MySqlPool;

/// Built-in calendars every company has: slug, name, color.
pub const SYSTEM_CALENDARS: [(&str, &str, &str); 3] = [
    ("templates", "Templates", "green"),
    ("installations", "Installations", "red"),
    ("estimates", "Estimates", "blue"),
];

/// Creates missing built-in calendars and restores their names (colors are
/// the company's choice). Port of `ensureSystemCalendars`.
pub async fn ensure_system_calendars(
    pool: &MySqlPool,
    company_id: i32,
    created_by: i32,
) -> Result<(), sqlx::Error> {
    let existing: Vec<(String, String)> = sqlx::query_as(
        "SELECT slug, name FROM calendars
         WHERE company_id = ? AND slug IN ('templates', 'installations', 'estimates') AND deleted_at IS NULL",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    for (slug, name, color) in SYSTEM_CALENDARS {
        let rows: Vec<&String> = existing
            .iter()
            .filter(|(s, _)| s == slug)
            .map(|(_, n)| n)
            .collect();
        if rows.is_empty() {
            sqlx::query(
                "INSERT INTO calendars (company_id, name, color, slug, created_by)
                 SELECT ?, ?, ?, ?, ? WHERE NOT EXISTS (
                   SELECT 1 FROM calendars WHERE company_id = ? AND slug = ? AND deleted_at IS NULL)",
            )
            .bind(company_id)
            .bind(name)
            .bind(color)
            .bind(slug)
            .bind(created_by)
            .bind(company_id)
            .bind(slug)
            .execute(pool)
            .await?;
        } else if rows.iter().any(|n| n.as_str() != name) {
            sqlx::query(
                "UPDATE calendars SET name = ?, updated_at = CURRENT_TIMESTAMP
                 WHERE company_id = ? AND slug = ? AND deleted_at IS NULL",
            )
            .bind(name)
            .bind(company_id)
            .bind(slug)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Live calendars not hidden for the company nor from `user_id`: built-in
/// ones first (templates, installations, estimates), then by name.
pub async fn visible_calendars(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> Result<Vec<Calendar>, sqlx::Error> {
    sqlx::query_as(
        "SELECT c.id, c.name, c.color, c.slug,
                COALESCE(c.slug IN ('templates', 'installations', 'estimates'), 0) AS is_system
         FROM calendars c
         WHERE c.company_id = ? AND c.deleted_at IS NULL AND c.is_hidden = 0
           AND NOT EXISTS (SELECT 1 FROM calendar_hidden_users h
                           WHERE h.calendar_id = c.id AND h.company_id = c.company_id AND h.user_id = ?)
         ORDER BY CASE c.slug WHEN 'templates' THEN 1 WHEN 'installations' THEN 2
                              WHEN 'estimates' THEN 3 ELSE 4 END, c.name, c.id",
    )
    .bind(company_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
}
