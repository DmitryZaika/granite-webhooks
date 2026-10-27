//! Which sales reps' deals a company drip goes to: `sales_rep_ids`, a JSON
//! array of `users.id` (NULL or empty: every rep's deals).

use crate::error::{ApiError, ApiResult};
use sqlx::{AssertSqlSafe, MySqlPool};

/// The stored JSON as ids; empty: every rep.
pub fn ids_of(json: Option<&str>) -> Vec<i32> {
    json.and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default()
}

/// The ids as stored; NULL for every rep.
pub fn to_json(ids: &[i32]) -> Option<String> {
    (!ids.is_empty()).then(|| serde_json::to_string(ids).unwrap_or_default())
}

/// Two drips reach the same deals: one goes to every rep, or they share one.
pub fn overlap(a: &[i32], b: &[i32]) -> bool {
    a.is_empty() || b.is_empty() || a.iter().any(|id| b.contains(id))
}

/// The picked reps, sorted and deduped; each a live user of the company.
pub async fn checked(pool: &MySqlPool, company_id: i32, ids: &[i32]) -> ApiResult<Vec<i32>> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(ids);
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let mut query = sqlx::query_scalar::<_, i64>(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM users
          WHERE company_id = ? AND is_deleted = 0 AND id IN ({placeholders})"
    )))
    .bind(company_id);
    for id in &ids {
        query = query.bind(id);
    }
    if usize::try_from(query.fetch_one(pool).await?).ok() != Some(ids.len()) {
        return Err(ApiError::BadRequest(
            "Pick sales reps who work at your company.".into(),
        ));
    }
    Ok(ids)
}

/// Where a template's pending automatic sends wait.
#[derive(Debug, Clone, Copy)]
pub enum Queue {
    Emails,
    Sms,
}

/// Pending sends of the template for deals of reps it no longer names are
/// called off; nothing when it goes to every rep.
pub async fn cancel_for_other_reps(
    pool: &MySqlPool,
    queue: Queue,
    template_id: i32,
    ids: &[i32],
) -> Result<(), sqlx::Error> {
    let Some(json) = to_json(ids) else {
        return Ok(());
    };
    let table = match queue {
        Queue::Emails => "scheduled_emails",
        Queue::Sms => "scheduled_sms",
    };
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {table} s JOIN deals d ON d.id = s.deal_id
            SET s.status = 'cancelled', s.error_message = 'Template not for this sales rep'
          WHERE s.template_id = ? AND s.status = 'pending'
            AND NOT IFNULL(JSON_CONTAINS(?, CAST(d.user_id AS JSON)), 0)"
    )))
    .bind(template_id)
    .bind(json)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reps_overlap() {
        assert!(overlap(&[], &[4]));
        assert!(overlap(&[4, 5], &[]));
        assert!(overlap(&[4, 5], &[5, 8]));
        assert!(!overlap(&[4], &[5, 8]));
    }

    #[test]
    fn stored_ids() {
        assert_eq!(ids_of(Some("[4, 8]")), vec![4, 8]);
        assert_eq!(ids_of(None), Vec::<i32>::new());
        assert_eq!(ids_of(Some("oops")), Vec::<i32>::new());
        assert_eq!(to_json(&[]), None);
        assert_eq!(to_json(&[4, 8]).as_deref(), Some("[4,8]"));
    }
}
