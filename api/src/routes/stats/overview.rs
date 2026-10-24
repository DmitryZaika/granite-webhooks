//! `GET /v1/stats/overview`: the admin Statistics page's numbers.
//!
//! Replaces the Remix `admin.statistics` loader's queries and
//! `statisticsExtendedMetrics.server.ts`. Dates are `YYYY-MM-DD` days of the
//! stored datetimes (no time zone conversion), as the page always counted.

use crate::auth::AdminUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::routes::stats::schemas::SmsProvider;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySql, MySqlPool, QueryBuilder};
use utoipa::{IntoParams, ToSchema};

/// `positions.id` of a sales rep.
const SALES_REP: i32 = 1;

/// Query of `GET /v1/stats/overview`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OverviewQuery {
    /// First day counted (`YYYY-MM-DD`); every day when empty.
    #[serde(default)]
    pub from_date: String,
    /// Last day counted (`YYYY-MM-DD`); every day when empty.
    #[serde(default)]
    pub to_date: String,
    /// A sales rep's name; everyone when empty or `All`.
    #[serde(default)]
    pub sales_rep: String,
}

/// The filters, with the days in order.
struct Filters {
    from: Option<String>,
    to: Option<String>,
    rep: Option<String>,
}

impl Filters {
    fn new(query: &OverviewQuery) -> Self {
        let day = |value: &str| {
            let value = value.trim();
            chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .map(|_| value.to_string())
        };
        let (mut from, mut to) = (day(&query.from_date), day(&query.to_date));
        if let (Some(start), Some(end)) = (&from, &to)
            && start > end
        {
            std::mem::swap(&mut from, &mut to);
        }
        let rep = query.sales_rep.trim();
        Self {
            from,
            to,
            rep: (!rep.is_empty() && rep != "All").then(|| rep.to_string()),
        }
    }

    /// ` AND DATE(<column>) >= ? AND DATE(<column>) <= ?` for the set days.
    fn days(&self, sql: &mut QueryBuilder<MySql>, column: &str) {
        if let Some(from) = &self.from {
            sql.push(format!(" AND DATE({column}) >= "))
                .push_bind(from.clone());
        }
        if let Some(to) = &self.to {
            sql.push(format!(" AND DATE({column}) <= "))
                .push_bind(to.clone());
        }
    }

    /// ` AND <column> = ?` for the rep.
    fn rep(&self, sql: &mut QueryBuilder<MySql>, column: &str) {
        if let Some(rep) = &self.rep {
            sql.push(format!(" AND {column} = ")).push_bind(rep.clone());
        }
    }
}

/// Leads count from their assignment, other customers from their creation.
const CUSTOMER_COUNT_DATE: &str =
    "CASE WHEN c.source = 'leads' THEN c.assigned_date ELSE c.created_date END";

/// A rep's deals.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct DealsByRep {
    /// The rep (user id).
    pub rep_id: i32,
    /// The rep's name.
    pub rep_name: Option<String>,
    /// Deals counted.
    pub deals_count: i64,
    /// Average amount of deals with one.
    pub avg_amount: f64,
    /// Average amount of won deals with one.
    pub avg_amount_won: f64,
    /// Won deals.
    pub won_count: i64,
    /// Lost deals.
    pub lost_count: i64,
    /// Won out of closed, in percent.
    pub won_lost_ratio: f64,
    /// Open deals' amount.
    pub pipeline_amount: f64,
    /// Won deals of walk-in customers.
    pub won_count_walkin: i64,
    /// Lost deals of walk-in customers.
    pub lost_count_walkin: i64,
    /// Walk-in won out of closed, in percent.
    pub won_lost_ratio_walkin: f64,
    /// Won deals of leads.
    pub won_count_leads: i64,
    /// Lost deals of leads.
    pub lost_count_leads: i64,
    /// Leads won out of closed, in percent.
    pub won_lost_ratio_leads: f64,
}

/// Open deals in a stage.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct DealsByStage {
    /// The stage.
    pub list_id: i32,
    /// The stage's name.
    pub list_name: Option<String>,
    /// Deals counted.
    pub deals_count: i64,
    /// Their amounts summed.
    pub total_amount: f64,
}

/// A pipeline stage.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct DealsList {
    /// Id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Order on the board.
    pub position: Option<i32>,
}

/// The company's customers.
#[derive(Debug, Default, Serialize, FromRow, ToSchema)]
pub struct CustomersTotals {
    /// Customers.
    pub total: i64,
    /// Without a sales rep.
    pub without_rep: i64,
    /// Marked invalid.
    pub invalid: i64,
    /// Added in the last 30 days.
    pub last_30: i64,
}

/// Valid customers from a source.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct CustomersBySource {
    /// Customer source (`leads`, `check-in`, ...).
    pub source: Option<String>,
    /// Customers.
    pub total: i64,
}

/// A rep's valid customers by source.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct CustomersByRep {
    /// The rep's name.
    pub rep_name: Option<String>,
    /// Walk-ins.
    pub walkin: i64,
    /// Call-ins.
    pub callin: i64,
    /// Leads.
    pub leads: i64,
    /// Added by hand.
    pub manual: i64,
    /// Customers.
    pub total: i64,
}

/// A customer of the table.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct TableCustomer {
    /// Id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// UTC, ISO 8601.
    pub created_date: Option<String>,
    /// Customer source (`leads`, `check-in`, ...).
    pub source: Option<String>,
    /// How they heard of the company.
    pub referral_source: Option<String>,
    /// The lead form.
    pub form_name: Option<String>,
    /// Why the lead is invalid.
    pub invalid_lead: Option<String>,
    /// The sales rep's name.
    pub sales_rep_name: Option<String>,
}

/// A deal of the table's customers.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct TableDeal {
    /// Id.
    pub id: i64,
    /// The customer.
    pub customer_id: i32,
    /// Amount.
    pub amount: Option<f64>,
    /// Stage name.
    pub status: Option<String>,
    /// Why it was lost.
    pub lost_reason: Option<String>,
    /// 1 won, 0 lost, null open.
    pub is_won: Option<i32>,
    /// The sales rep's name.
    pub sales_rep_name: String,
}

/// A rep's deals made and sold, by customer source.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct ConversionMetrics {
    /// The rep's name.
    pub rep_name: Option<String>,
    /// Won deals.
    pub total_sold: i64,
    /// Deals made.
    pub total_created: i64,
    /// Leads' won deals.
    pub leads_sold_same_month: i64,
    /// Leads' deals.
    pub leads_created: i64,
    /// Walk-ins' won deals.
    pub walkin_sold_same_month: i64,
    /// Walk-ins' deals.
    pub walkin_created: i64,
    /// Call-ins' won deals.
    pub callin_sold_same_month: i64,
    /// Call-ins' deals.
    pub callin_created: i64,
}

/// How often a rep lost for a reason.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct LostReason {
    /// The rep's name.
    pub rep_name: Option<String>,
    /// Why it was lost.
    pub lost_reason: Option<String>,
    /// How many.
    pub count: i64,
}

/// A marketing campaign's customers.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct Campaign {
    /// The campaign (`customers.compaign_name`).
    pub compaign_name: Option<String>,
    /// Customers it brought.
    pub customers_acquired: i64,
    /// Won and lost deals.
    pub closed_deals: i64,
    /// Won deals.
    pub won_deals: i64,
    /// Won out of closed, in percent; null with no closed deals.
    pub conversion_percent: Option<f64>,
}

/// A rep's post-installation survey score.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct RepRating {
    /// The rep (user id).
    pub rep_id: i32,
    /// The rep's name.
    pub rep_name: Option<String>,
    /// Average rating (1-5).
    pub avg_rating: Option<f64>,
    /// Surveys answered.
    pub responses: i64,
}

/// A stone's sold slabs.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct TopSoldColor {
    /// The stone.
    pub stone_name: Option<String>,
    /// Slabs sold.
    pub slabs_sold: i64,
    /// Their square feet.
    pub square_feet: f64,
}

/// A month's count (`YYYY-MM`); months without any are left out.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct MonthCount {
    /// `YYYY-MM`.
    pub month: String,
    /// How many.
    pub count: i64,
}

/// `GET /v1/stats/overview`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Overview {
    /// The days counted, in order.
    pub from_date: Option<String>,
    /// Last day counted.
    pub to_date: Option<String>,
    /// Deals by rep.
    pub deals_by_rep: Vec<DealsByRep>,
    /// Open deals by stage.
    pub deals_by_stage: Vec<DealsByStage>,
    /// Stages of the shown pipelines.
    pub lists: Vec<DealsList>,
    /// Customer totals.
    pub customers_totals: CustomersTotals,
    /// Valid customers by source.
    pub customers_by_source: Vec<CustomersBySource>,
    /// Valid customers by rep.
    pub customers_by_rep: Vec<CustomersByRep>,
    /// Customers, newest first.
    pub customers_table: Vec<TableCustomer>,
    /// Those customers' deals.
    pub customers_deals: Vec<TableDeal>,
    /// Deals made and won by rep.
    pub conversion_metrics_by_rep: Vec<ConversionMetrics>,
    /// Lost reasons by rep.
    pub lost_reasons_by_rep: Vec<LostReason>,
    /// Top 8 campaigns.
    pub customers_by_campaign: Vec<Campaign>,
    /// Survey ratings by rep.
    pub sales_rep_ratings: Vec<RepRating>,
    /// Top 10 stones by slabs sold.
    pub top_sold_colors: Vec<TopSoldColor>,
    /// Emails sent per month.
    pub emails_per_month: Vec<MonthCount>,
    /// Null when the company has no SMS provider.
    pub sms_per_month: Option<Vec<MonthCount>>,
}

/// Won/lost counts and their ratio for deals matching `when`.
fn won_lost(when: &str, suffix: &str) -> String {
    let won = format!("SUM(CASE WHEN d.is_won = 1{when} THEN 1 ELSE 0 END)");
    let lost = format!("SUM(CASE WHEN d.is_won = 0{when} THEN 1 ELSE 0 END)");
    format!(
        "CAST(COALESCE({won}, 0) AS SIGNED) AS won_count{suffix},
         CAST(COALESCE({lost}, 0) AS SIGNED) AS lost_count{suffix},
         CAST(CASE WHEN COALESCE({won} + {lost}, 0) = 0 THEN 0
                   ELSE ROUND(100 * {won} / ({won} + {lost}), 0) END AS DOUBLE) AS won_lost_ratio{suffix}"
    )
}

async fn deals_by_rep(
    pool: &MySqlPool,
    company: i32,
    f: &Filters,
) -> sqlx::Result<Vec<DealsByRep>> {
    let mut sql = QueryBuilder::new(format!(
        "SELECT u.id AS rep_id, u.name AS rep_name, COUNT(d.id) AS deals_count,
                CAST(COALESCE(AVG(NULLIF(d.amount, 0)), 0) AS DOUBLE) AS avg_amount,
                CAST(COALESCE(
                  SUM(CASE WHEN d.is_won = 1 AND d.amount <> 0 THEN d.amount ELSE 0 END) /
                  NULLIF(SUM(CASE WHEN d.is_won = 1 AND d.amount <> 0 THEN 1 ELSE 0 END), 0),
                0) AS DOUBLE) AS avg_amount_won,
                {},
                CAST(COALESCE(SUM(CASE WHEN d.is_won IS NULL AND d.amount <> 0 THEN d.amount ELSE 0 END), 0) AS DOUBLE) AS pipeline_amount,
                {},
                {}
           FROM deals d
           JOIN users u ON d.user_id = u.id AND u.is_deleted = 0 AND u.company_id = ",
        won_lost("", ""),
        won_lost(" AND c.source = 'check-in'", "_walkin"),
        won_lost(" AND c.source = 'leads'", "_leads"),
    ));
    sql.push_bind(company).push(
        " JOIN deals_list l ON d.list_id = l.id
          JOIN customers c ON d.customer_id = c.id
         WHERE c.company_id = ",
    );
    sql.push_bind(company)
        .push(" AND c.deleted_at IS NULL AND d.deleted_at IS NULL AND l.deleted_at IS NULL");
    f.days(&mut sql, "d.updated_at");
    f.rep(&mut sql, "u.name");
    sql.push(" GROUP BY u.id, u.name ORDER BY deals_count DESC");
    sql.build_query_as().fetch_all(pool).await
}

async fn deals_by_stage(
    pool: &MySqlPool,
    company: i32,
    f: &Filters,
) -> sqlx::Result<Vec<DealsByStage>> {
    let mut sql = QueryBuilder::new(
        "SELECT l.id AS list_id, l.name AS list_name, COUNT(d.id) AS deals_count,
                CAST(COALESCE(SUM(d.amount), 0) AS DOUBLE) AS total_amount
           FROM deals d
           JOIN deals_list l ON d.list_id = l.id
           JOIN customers c ON d.customer_id = c.id
           JOIN users u ON d.user_id = u.id AND u.is_deleted = 0 AND u.company_id = ",
    );
    sql.push_bind(company).push(" WHERE c.company_id = ");
    sql.push_bind(company).push(
        " AND c.deleted_at IS NULL AND d.deleted_at IS NULL AND l.deleted_at IS NULL AND d.is_won IS NULL",
    );
    f.days(&mut sql, "d.updated_at");
    f.rep(&mut sql, "u.name");
    sql.push(" GROUP BY l.id, l.name, l.position ORDER BY l.position");
    sql.build_query_as().fetch_all(pool).await
}

/// Valid customers by source or by rep share these filters.
fn customer_filters(sql: &mut QueryBuilder<MySql>, company: i32, f: &Filters) {
    sql.push(" WHERE c.company_id = ").push_bind(company);
    sql.push(" AND c.deleted_at IS NULL");
    f.days(sql, CUSTOMER_COUNT_DATE);
    f.rep(sql, "u.name");
}

async fn customers_by_source(
    pool: &MySqlPool,
    company: i32,
    f: &Filters,
) -> sqlx::Result<Vec<CustomersBySource>> {
    let mut sql = QueryBuilder::new("SELECT c.source, COUNT(*) AS total FROM customers c");
    if f.rep.is_some() {
        sql.push(
            " JOIN users u ON c.sales_rep = u.id AND u.is_deleted = 0 AND u.company_id = c.company_id",
        );
    }
    customer_filters(&mut sql, company, f);
    sql.push(" AND (c.invalid_lead IS NULL OR c.invalid_lead = '') GROUP BY c.source ORDER BY total DESC");
    sql.build_query_as().fetch_all(pool).await
}

async fn customers_by_rep(
    pool: &MySqlPool,
    company: i32,
    f: &Filters,
) -> sqlx::Result<Vec<CustomersByRep>> {
    let mut sql = QueryBuilder::new(format!(
        "SELECT u.name AS rep_name,
                CAST(SUM(CASE WHEN c.source = 'check-in' THEN 1 ELSE 0 END) AS SIGNED) AS walkin,
                CAST(SUM(CASE WHEN c.source = 'call-in' THEN 1 ELSE 0 END) AS SIGNED) AS callin,
                CAST(SUM(CASE WHEN c.source = 'leads' THEN 1 ELSE 0 END) AS SIGNED) AS leads,
                CAST(SUM(CASE WHEN c.source IN ('other', 'user-input') THEN 1 ELSE 0 END) AS SIGNED) AS `manual`,
                COUNT(c.id) AS total
           FROM customers c
           JOIN users u ON c.sales_rep = u.id AND u.is_deleted = 0 AND u.company_id = c.company_id
           JOIN users_positions up ON up.user_id = u.id AND up.position_id = {SALES_REP}"
    ));
    customer_filters(&mut sql, company, f);
    sql.push(
        " AND (c.invalid_lead IS NULL OR c.invalid_lead = '') GROUP BY u.name ORDER BY total DESC",
    );
    sql.build_query_as().fetch_all(pool).await
}

async fn customers_table(
    pool: &MySqlPool,
    company: i32,
    f: &Filters,
) -> sqlx::Result<Vec<TableCustomer>> {
    let mut sql = QueryBuilder::new(
        "SELECT c.id, c.name, DATE_FORMAT(c.created_date, '%Y-%m-%dT%H:%i:%sZ') AS created_date,
                c.source, c.referral_source, c.form_name, c.invalid_lead, u.name AS sales_rep_name
           FROM customers c
           LEFT JOIN users u ON c.sales_rep = u.id AND u.is_deleted = 0 AND u.company_id = c.company_id",
    );
    customer_filters(&mut sql, company, f);
    sql.push(" ORDER BY c.created_date DESC, c.id DESC");
    sql.build_query_as().fetch_all(pool).await
}

async fn customers_deals(
    pool: &MySqlPool,
    company: i32,
    f: &Filters,
) -> sqlx::Result<Vec<TableDeal>> {
    let mut sql = QueryBuilder::new(
        "SELECT CAST(d.id AS SIGNED) AS id, d.customer_id, CAST(d.amount AS DOUBLE) AS amount,
                d.status, d.lost_reason, d.is_won, COALESCE(u.name, u2.name, '') AS sales_rep_name
           FROM deals d
           JOIN customers c ON d.customer_id = c.id
           LEFT JOIN users u ON d.user_id = u.id AND u.is_deleted = 0 AND u.company_id = c.company_id
           LEFT JOIN users u2 ON c.sales_rep = u2.id AND u2.is_deleted = 0 AND u2.company_id = c.company_id
          WHERE c.company_id = ",
    );
    sql.push_bind(company)
        .push(" AND c.deleted_at IS NULL AND d.deleted_at IS NULL");
    f.days(&mut sql, "d.created_at");
    if let Some(rep) = &f.rep {
        sql.push(" AND (u.name = ")
            .push_bind(rep.clone())
            .push(" OR u2.name = ")
            .push_bind(rep.clone())
            .push(")");
    }
    sql.push(" ORDER BY d.created_at DESC, d.id DESC");
    sql.build_query_as().fetch_all(pool).await
}

async fn conversion_metrics(
    pool: &MySqlPool,
    company: i32,
    f: &Filters,
) -> sqlx::Result<Vec<ConversionMetrics>> {
    let mut sql = QueryBuilder::new(
        "SELECT u.name AS rep_name,
                COUNT(CASE WHEN d.is_won = 1 THEN 1 END) AS total_sold,
                COUNT(*) AS total_created,
                COUNT(CASE WHEN c.source = 'leads' AND d.is_won = 1 THEN 1 END) AS leads_sold_same_month,
                COUNT(CASE WHEN c.source = 'leads' THEN 1 END) AS leads_created,
                COUNT(CASE WHEN c.source = 'check-in' AND d.is_won = 1 THEN 1 END) AS walkin_sold_same_month,
                COUNT(CASE WHEN c.source = 'check-in' THEN 1 END) AS walkin_created,
                COUNT(CASE WHEN c.source = 'call-in' AND d.is_won = 1 THEN 1 END) AS callin_sold_same_month,
                COUNT(CASE WHEN c.source = 'call-in' THEN 1 END) AS callin_created
           FROM deals d
           JOIN customers c ON d.customer_id = c.id
           JOIN deals_list dl ON d.list_id = dl.id
           JOIN users u ON d.user_id = u.id AND u.is_deleted = 0 AND u.company_id = ",
    );
    sql.push_bind(company).push(" WHERE c.company_id = ");
    sql.push_bind(company)
        .push(" AND c.deleted_at IS NULL AND d.deleted_at IS NULL AND dl.deleted_at IS NULL");
    f.days(&mut sql, "d.created_at");
    f.rep(&mut sql, "u.name");
    sql.push(" GROUP BY u.name ORDER BY total_sold DESC");
    sql.build_query_as().fetch_all(pool).await
}

async fn lost_reasons(
    pool: &MySqlPool,
    company: i32,
    f: &Filters,
) -> sqlx::Result<Vec<LostReason>> {
    let mut sql = QueryBuilder::new(
        "SELECT u.name AS rep_name, d.lost_reason, COUNT(d.id) AS count
           FROM deals d
           JOIN users u ON d.user_id = u.id AND u.is_deleted = 0 AND u.company_id = ",
    );
    sql.push_bind(company).push(
        " JOIN deals_list l ON d.list_id = l.id
          JOIN customers c ON d.customer_id = c.id
         WHERE c.company_id = ",
    );
    sql.push_bind(company).push(
        " AND c.deleted_at IS NULL AND d.deleted_at IS NULL AND l.deleted_at IS NULL
          AND d.is_won = 0 AND d.lost_reason IS NOT NULL AND d.lost_reason <> ''",
    );
    f.days(&mut sql, "d.updated_at");
    f.rep(&mut sql, "u.name");
    sql.push(" GROUP BY u.name, d.lost_reason ORDER BY u.name, count DESC");
    sql.build_query_as().fetch_all(pool).await
}

async fn campaigns(pool: &MySqlPool, company: i32, f: &Filters) -> sqlx::Result<Vec<Campaign>> {
    let mut sql = QueryBuilder::new(
        "SELECT c.compaign_name,
                COUNT(DISTINCT c.id) AS customers_acquired,
                COUNT(DISTINCT CASE WHEN d.is_won IS NOT NULL THEN d.id END) AS closed_deals,
                COUNT(DISTINCT CASE WHEN d.is_won = 1 THEN d.id END) AS won_deals,
                CAST(ROUND(100.0 * COUNT(DISTINCT CASE WHEN d.is_won = 1 THEN d.id END) /
                  NULLIF(COUNT(DISTINCT CASE WHEN d.is_won IS NOT NULL THEN d.id END), 0), 1) AS DOUBLE)
                  AS conversion_percent
           FROM customers c
           LEFT JOIN deals d ON d.customer_id = c.id AND d.deleted_at IS NULL",
    );
    if f.rep.is_some() {
        sql.push(
            " JOIN users u ON d.user_id = u.id AND u.is_deleted = 0 AND u.company_id = c.company_id",
        );
    }
    sql.push(" WHERE c.company_id = ").push_bind(company).push(
        " AND c.deleted_at IS NULL AND (c.invalid_lead IS NULL OR c.invalid_lead = '')
          AND c.compaign_name IS NOT NULL AND TRIM(c.compaign_name) <> ''",
    );
    f.days(&mut sql, "c.created_date");
    f.rep(&mut sql, "u.name");
    sql.push(" GROUP BY c.compaign_name ORDER BY customers_acquired DESC LIMIT 8");
    sql.build_query_as().fetch_all(pool).await
}

async fn ratings(pool: &MySqlPool, company: i32, f: &Filters) -> sqlx::Result<Vec<RepRating>> {
    let mut sql = QueryBuilder::new(
        "SELECT cs.sales_rep_id AS rep_id, u.name AS rep_name,
                CAST(ROUND(AVG(cs.sales_rep_rating), 2) AS DOUBLE) AS avg_rating,
                COUNT(*) AS responses
           FROM customer_surveys cs
           JOIN users u ON cs.sales_rep_id = u.id
          WHERE cs.company_id = ",
    );
    sql.push_bind(company)
        .push(" AND u.company_id = ")
        .push_bind(company)
        .push(" AND u.is_deleted = 0");
    f.days(&mut sql, "cs.created_at");
    f.rep(&mut sql, "u.name");
    sql.push(" GROUP BY cs.sales_rep_id, u.name ORDER BY avg_rating DESC");
    sql.build_query_as().fetch_all(pool).await
}

async fn top_sold_colors(
    pool: &MySqlPool,
    company: i32,
    f: &Filters,
) -> sqlx::Result<Vec<TopSoldColor>> {
    let mut sql = QueryBuilder::new(
        "SELECT st.name AS stone_name, COUNT(si.id) AS slabs_sold,
                CAST(COALESCE(SUM(si.square_feet), 0) AS DOUBLE) AS square_feet
           FROM sales s
           JOIN slab_inventory si ON si.sale_id = s.id AND si.deleted_at IS NULL AND si.parent_id IS NULL
           JOIN stones st ON st.id = si.stone_id AND st.deleted_at IS NULL AND st.company_id = s.company_id
           JOIN users u ON u.id = s.seller_id AND u.is_deleted = 0 AND u.company_id = s.company_id
          WHERE s.company_id = ",
    );
    sql.push_bind(company).push(" AND s.cancelled_date IS NULL");
    f.days(&mut sql, "s.sale_date");
    f.rep(&mut sql, "u.name");
    sql.push(" GROUP BY st.id, st.name ORDER BY slabs_sold DESC, square_feet DESC LIMIT 10");
    sql.build_query_as().fetch_all(pool).await
}

async fn emails_per_month(
    pool: &MySqlPool,
    company: i32,
    f: &Filters,
) -> sqlx::Result<Vec<MonthCount>> {
    let mut sql = QueryBuilder::new(
        "SELECT DATE_FORMAT(e.sent_at, '%Y-%m') AS month, COUNT(*) AS count
           FROM emails e
           JOIN users u ON u.id = e.sender_user_id AND u.is_deleted = 0 AND u.company_id = ",
    );
    sql.push_bind(company)
        .push(" WHERE e.deleted_at IS NULL AND e.sender_user_id IS NOT NULL AND (e.company_id = ")
        .push_bind(company)
        .push(" OR e.company_id IS NULL)");
    f.days(&mut sql, "e.sent_at");
    f.rep(&mut sql, "u.name");
    sql.push(" GROUP BY month ORDER BY month");
    sql.build_query_as().fetch_all(pool).await
}

async fn sms_per_month_of(
    pool: &MySqlPool,
    company: i32,
    provider: SmsProvider,
    f: &Filters,
) -> sqlx::Result<Vec<MonthCount>> {
    // A Telnyx text's agent is its sender's user id.
    let table = provider.sms_table();
    let agent = provider.agent_column().map_or_else(
        || "CAST(ru.id AS CHAR)".to_string(),
        |column| format!("ru.{column}"),
    );
    let mut sql = QueryBuilder::new(format!(
        "SELECT DATE_FORMAT(cs.created_date, '%Y-%m') AS month, COUNT(*) AS count
           FROM {table} cs
           LEFT JOIN users u ON u.id = cs.sender_user_id AND u.is_deleted = 0
          WHERE cs.company_id = "
    ));
    sql.push_bind(company)
        .push(" AND cs.direction = 'outbound' AND cs.status = 'sent'");
    f.days(&mut sql, "cs.created_date");
    if let Some(rep) = &f.rep {
        sql.push(" AND (u.name = ")
            .push_bind(rep.clone())
            .push(format!(
                " OR EXISTS (SELECT 1 FROM users ru WHERE ru.company_id = cs.company_id
                           AND ru.is_deleted = 0 AND {agent} = cs.agent AND ru.name = "
            ));
        sql.push_bind(rep.clone()).push("))");
    }
    sql.push(" GROUP BY month ORDER BY month");
    sql.build_query_as().fetch_all(pool).await
}

/// The Statistics page
///
/// Deals by rep and by stage, customers by source and by rep, the customers
/// table with its deals, conversion by rep, lost reasons, campaigns, survey
/// ratings, top sold stones and emails and texts per month, for the
/// company's admins. Deals count by their last update (conversion and the
/// table by creation), leads by assignment and other customers by creation.
#[utoipa::path(
    get,
    path = "/v1/stats/overview",
    operation_id = "get_stats_overview",
    tag = "stats",
    params(OverviewQuery),
    responses(
        (status = 200, description = "The numbers", body = Overview),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn overview(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    MultiQuery(query): MultiQuery<OverviewQuery>,
) -> ApiResult<Json<Overview>> {
    let pool = &state.pool;
    let company = user.company_id;
    let f = Filters::new(&query);
    let features = crate::routes::company::queries::company_features(pool, company).await?;
    let customers_totals: CustomersTotals = sqlx::query_as(
        "SELECT COUNT(*) AS total,
                CAST(COALESCE(SUM(CASE WHEN sales_rep IS NULL THEN 1 ELSE 0 END), 0) AS SIGNED) AS without_rep,
                CAST(COALESCE(SUM(CASE WHEN invalid_lead IS NOT NULL AND invalid_lead <> '' THEN 1 ELSE 0 END), 0) AS SIGNED) AS invalid,
                CAST(COALESCE(SUM(CASE WHEN created_date >= (CURRENT_DATE - INTERVAL 30 DAY) THEN 1 ELSE 0 END), 0) AS SIGNED) AS last_30
           FROM customers WHERE company_id = ? AND deleted_at IS NULL",
    )
    .bind(company)
    .fetch_one(pool)
    .await?;
    let lists: Vec<DealsList> = sqlx::query_as(
        "SELECT dl.id, dl.name, dl.position
           FROM deals_list dl JOIN groups_list gl ON dl.group_id = gl.id
          WHERE dl.deleted_at IS NULL AND gl.deleted_at IS NULL AND gl.is_displayed = 1
            AND (gl.company_id = ? OR gl.id = 1)
          ORDER BY dl.position",
    )
    .bind(company)
    .fetch_all(pool)
    .await?;
    let mut sms_per_month = match features.sms_provider() {
        Some(provider) => Some(sms_per_month_of(pool, company, provider, &f).await?),
        None => None,
    };
    // Texts through Telnyx count too, month by month.
    if features.telnyx_sms {
        let mut months = sms_per_month.take().unwrap_or_default();
        for telnyx in sms_per_month_of(pool, company, SmsProvider::Telnyx, &f).await? {
            match months.iter_mut().find(|month| month.month == telnyx.month) {
                Some(month) => month.count += telnyx.count,
                None => months.push(telnyx),
            }
        }
        months.sort_by(|a, b| a.month.cmp(&b.month));
        sms_per_month = Some(months);
    }
    Ok(Json(Overview {
        deals_by_rep: deals_by_rep(pool, company, &f).await?,
        deals_by_stage: deals_by_stage(pool, company, &f).await?,
        lists,
        customers_totals,
        customers_by_source: customers_by_source(pool, company, &f).await?,
        customers_by_rep: customers_by_rep(pool, company, &f).await?,
        customers_table: customers_table(pool, company, &f).await?,
        customers_deals: customers_deals(pool, company, &f).await?,
        conversion_metrics_by_rep: conversion_metrics(pool, company, &f).await?,
        lost_reasons_by_rep: lost_reasons(pool, company, &f).await?,
        customers_by_campaign: campaigns(pool, company, &f).await?,
        sales_rep_ratings: ratings(pool, company, &f).await?,
        top_sold_colors: top_sold_colors(pool, company, &f).await?,
        emails_per_month: emails_per_month(pool, company, &f).await?,
        sms_per_month,
        from_date: f.from,
        to_date: f.to,
    }))
}
