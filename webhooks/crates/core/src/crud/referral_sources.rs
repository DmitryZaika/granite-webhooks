use sqlx::MySqlPool;

/// Names in the company's referral source catalog (the list managed from the
/// CRM's "Add lead" page).
pub async fn get_marketing_referral_source_names(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT name
           FROM marketing_refferal_sources
           WHERE company_id = ? AND deleted_at IS NULL
           ORDER BY name ASC"#,
        company_id
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferralAlias {
    pub resolved_value: Option<String>,
    pub ai_certain: Option<bool>,
    pub reason: Option<String>,
    pub times_seen: i32,
}

/// Counts one more lead with this unrecognized value and returns what is
/// remembered about it (creating the row the first time).
pub async fn record_referral_alias_seen(
    pool: &MySqlPool,
    company_id: i32,
    raw_key: &str,
) -> Result<ReferralAlias, sqlx::Error> {
    sqlx::query!(
        r#"INSERT INTO referral_source_aliases (company_id, raw_key, times_seen)
           VALUES (?, ?, 1)
           ON DUPLICATE KEY UPDATE times_seen = times_seen + 1"#,
        company_id,
        raw_key
    )
    .execute(pool)
    .await?;
    sqlx::query_as!(
        ReferralAlias,
        r#"SELECT resolved_value, ai_certain AS "ai_certain: bool", reason, times_seen
           FROM referral_source_aliases
           WHERE company_id = ? AND raw_key = ?"#,
        company_id,
        raw_key
    )
    .fetch_one(pool)
    .await
}

pub async fn save_referral_alias_decision(
    pool: &MySqlPool,
    company_id: i32,
    raw_key: &str,
    resolved_value: Option<&str>,
    ai_certain: Option<bool>,
    reason: &str,
) -> Result<(), sqlx::Error> {
    let reason: String = reason.chars().take(500).collect();
    sqlx::query!(
        r#"UPDATE referral_source_aliases
           SET resolved_value = ?, ai_certain = ?, reason = ?
           WHERE company_id = ? AND raw_key = ?"#,
        resolved_value,
        ai_certain,
        reason,
        company_id,
        raw_key
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// True when no review email went out for this value in the last 24 hours;
/// marks it as notified in the same statement, so parallel leads email once.
pub async fn claim_referral_alias_notification(
    pool: &MySqlPool,
    company_id: i32,
    raw_key: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"UPDATE referral_source_aliases
           SET last_notified_at = CURRENT_TIMESTAMP
           WHERE company_id = ? AND raw_key = ?
             AND (last_notified_at IS NULL OR last_notified_at < CURRENT_TIMESTAMP - INTERVAL 1 DAY)"#,
        company_id,
        raw_key
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}
