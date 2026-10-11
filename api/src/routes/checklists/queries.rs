use super::schemas::Checklist;
use sqlx::MySqlPool;

/// Remix: the loader of `employee.checklists.tsx`. The signature image is
/// left out: it is only needed for the PDF.
pub async fn list_checklists(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<Checklist>, sqlx::Error> {
    sqlx::query_as::<_, Checklist>(
        "SELECT
            c.id, c.customer_id, c.customer_name, c.installation_address, c.email,
            c.installer_id, u.name AS installer_name,
            c.material_correct, c.seams_satisfaction, c.appliances_fit,
            c.backsplashes_correct, c.edges_correct, c.holes_drilled, c.cleanup_completed,
            c.comments, c.created_at
         FROM checklists c
         LEFT JOIN users u ON u.id = c.installer_id
         WHERE c.company_id = ?
         ORDER BY c.created_at DESC, c.id DESC",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await
}
