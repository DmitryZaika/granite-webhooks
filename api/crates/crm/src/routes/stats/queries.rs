use super::schemas::{ConversionRow, MonthlyCount, SalesStats, SmsProvider, TopSoldStone};
use crate::period::MonthToDate;
use chrono::NaiveDate;
// Dynamic SQL here only splices constants and fixed table names.
use sqlx::{AssertSqlSafe, MySqlPool};

/// Deals of `user_id` that count at all: not deleted, of a live customer of
/// the company, in a live list. Binds: user id, company id.
const LIVE_DEALS: &str = "
      FROM deals d
      JOIN customers c ON c.id = d.customer_id
      JOIN deals_list l ON l.id = d.list_id
     WHERE d.user_id = ?
       AND c.company_id = ?
       AND d.deleted_at IS NULL
       AND c.deleted_at IS NULL
       AND l.deleted_at IS NULL";

/// Remix: `loadSalesRepMonthlyStats` (`salesRepStats.server.ts`), the parts
/// the dashboard shows. Won/lost deals are dated by `updated_at` (set when a
/// deal is marked won or lost), new deals by `created_at`.
pub async fn sales_stats(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
    month: MonthToDate,
) -> Result<SalesStats, sqlx::Error> {
    // `this_month(col)` binds the month's first and last day.
    let this_month = |column: &str| format!("DATE({column}) BETWEEN ? AND ?");
    let won = format!("d.is_won = 1 AND {}", this_month("d.updated_at"));
    let lost = format!("d.is_won = 0 AND {}", this_month("d.updated_at"));
    let created = this_month("d.created_at");
    let sql = format!(
        "SELECT ? AS start_date, ? AS end_date,
                COUNT(CASE WHEN {won} THEN 1 END) AS won_count,
                COUNT(CASE WHEN {lost} THEN 1 END) AS lost_count,
                {won_amount} AS won_amount,
                COUNT(CASE WHEN d.is_won IS NULL THEN 1 END) AS active_deals_count,
                {pipeline_amount} AS pipeline_amount,
                COUNT(CASE WHEN {created} THEN 1 END) AS new_deals_count
         {LIVE_DEALS}",
        won_amount = decimal_text(&format!("SUM(CASE WHEN {won} THEN d.amount END)")),
        pipeline_amount = decimal_text("SUM(CASE WHEN d.is_won IS NULL THEN d.amount END)"),
    );
    let mut query = sqlx::query_as::<_, SalesStats>(AssertSqlSafe(sql))
        .bind(month.start.to_string())
        .bind(month.end.to_string());
    // won_count, lost_count, won_amount, new_deals_count, in that order.
    for _ in 0..4 {
        query = query.bind(month.start).bind(month.end);
    }
    query.bind(user_id).bind(company_id).fetch_one(pool).await
}

/// A sum of money as a decimal string, "0.00" when there is nothing to sum.
fn decimal_text(sum: &str) -> String {
    format!("CAST(CAST(COALESCE({sum}, 0) AS DECIMAL(14, 2)) AS CHAR)")
}

/// Remix: the conversion query of `loadEmployeeDashboardMetrics`.
pub async fn deal_conversion(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
) -> Result<ConversionRow, sqlx::Error> {
    let sql = format!(
        "SELECT COUNT(CASE WHEN d.is_won = 1 THEN 1 END) AS total_won,
                COUNT(*) AS total_created,
                COUNT(CASE WHEN c.source = 'leads' AND d.is_won = 1 THEN 1 END) AS leads_won,
                COUNT(CASE WHEN c.source = 'leads' THEN 1 END) AS leads_created,
                COUNT(CASE WHEN c.source = 'check-in' AND d.is_won = 1 THEN 1 END) AS walk_in_won,
                COUNT(CASE WHEN c.source = 'check-in' THEN 1 END) AS walk_in_created,
                COUNT(CASE WHEN c.source = 'call-in' AND d.is_won = 1 THEN 1 END) AS call_in_won,
                COUNT(CASE WHEN c.source = 'call-in' THEN 1 END) AS call_in_created
         {LIVE_DEALS}"
    );
    sqlx::query_as::<_, ConversionRow>(AssertSqlSafe(sql))
        .bind(user_id)
        .bind(company_id)
        .fetch_one(pool)
        .await
}

/// Remix: the top sold colors query of `loadEmployeeDashboardMetrics`.
///
/// Sales
/// the user made this month (not cancelled), their full slabs (remnants and
/// deleted slabs left out), grouped by live stone of the company.
pub async fn top_sold_stones(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
    month: MonthToDate,
) -> Result<Vec<TopSoldStone>, sqlx::Error> {
    sqlx::query_as::<_, TopSoldStone>(
        r"
        SELECT st.id AS stone_id, st.name AS stone_name,
               COUNT(si.id) AS slabs_sold,
               CAST(CAST(COALESCE(SUM(si.square_feet), 0) AS DECIMAL(14, 2)) AS CHAR)
                 AS square_feet
          FROM sales s
          JOIN slab_inventory si
            ON si.sale_id = s.id AND si.deleted_at IS NULL AND si.parent_id IS NULL
          JOIN stones st
            ON st.id = si.stone_id AND st.deleted_at IS NULL AND st.company_id = s.company_id
         WHERE s.company_id = ?
           AND s.seller_id = ?
           AND s.cancelled_date IS NULL
           AND DATE(s.sale_date) BETWEEN ? AND ?
         GROUP BY st.id
         ORDER BY slabs_sold DESC, SUM(si.square_feet) DESC, st.id
         LIMIT 3
        ",
    )
    .bind(company_id)
    .bind(user_id)
    .bind(month.start)
    .bind(month.end)
    .fetch_all(pool)
    .await
}

/// Remix: the emails per month query of `loadEmployeeDashboardMetrics`. Emails
/// the user sent from `from` through `to`, of the company or with no company.
pub async fn sent_emails_by_month(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<MonthlyCount>, sqlx::Error> {
    sqlx::query_as::<_, MonthlyCount>(
        r"
        SELECT DATE_FORMAT(e.sent_at, '%Y-%m') AS month, COUNT(*) AS count
          FROM emails e
         WHERE e.deleted_at IS NULL
           AND e.sender_user_id = ?
           AND (e.company_id = ? OR e.company_id IS NULL)
           AND DATE(e.sent_at) BETWEEN ? AND ?
         GROUP BY month
         ORDER BY month
        ",
    )
    .bind(user_id)
    .bind(company_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
}

/// Remix: the SMS per month query of `loadEmployeeDashboardMetrics`. Sent
/// outbound texts of the company's provider table from `from` through `to`,
/// sent by the user or through the user's agent / extension id.
pub async fn sent_sms_by_month(
    pool: &MySqlPool,
    provider: SmsProvider,
    user_id: i32,
    company_id: i32,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<MonthlyCount>, sqlx::Error> {
    // Table and column come from the provider, never from input; a Telnyx
    // text's agent is its sender's user id.
    let table = provider.sms_table();
    let agent = provider.agent_column().map_or_else(
        || "CAST(u.id AS CHAR)".to_string(),
        |column| format!("u.{column}"),
    );
    let sql = format!(
        "SELECT DATE_FORMAT(cs.created_date, '%Y-%m') AS month, COUNT(*) AS count
           FROM {table} cs
          WHERE cs.company_id = ?
            AND cs.direction = 'outbound'
            AND cs.status = 'sent'
            AND DATE(cs.created_date) BETWEEN ? AND ?
            AND (cs.sender_user_id = ?
                 OR EXISTS (SELECT 1 FROM users u
                             WHERE u.id = ? AND u.is_deleted = 0
                               AND {agent} IS NOT NULL
                               AND {agent} = cs.agent))
          GROUP BY month
          ORDER BY month"
    );
    sqlx::query_as::<_, MonthlyCount>(AssertSqlSafe(sql))
        .bind(company_id)
        .bind(from)
        .bind(to)
        .bind(user_id)
        .bind(user_id)
        .fetch_all(pool)
        .await
}
