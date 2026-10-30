//! `GET /v1/sales/daily`: the company's sales of a day, as the dashboard's
//! Daily sales widget lists them.

use super::contract::HALF_DEPOSIT_OVER;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::{IntoParams, ToSchema};
use crate::routes::customers::signatures::{SignedFile, view_url};
use crate::serde_helpers::js_date_required;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, MySqlPool, QueryBuilder};
use std::collections::HashMap;

/// Longest range one call may ask for.
const MAX_DAYS: i64 = 31;

/// Query of `GET /v1/sales/daily`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DailySalesQuery {
    /// First instant counted, e.g. the caller's local midnight in UTC
    /// (`2026-10-28T04:00:00Z`).
    pub from: DateTime<Utc>,
    /// End of the range, not counted (the next local midnight). At most 31
    /// days after `from`.
    pub to: DateTime<Utc>,
}

/// One sale of the day.
#[derive(Debug, Serialize, ToSchema)]
pub struct DailySale {
    /// Sale id; its sales order is `GET /v1/sales/{id}/contract`.
    pub id: i32,
    /// When it was sold.
    #[serde(serialize_with = "js_date_required")]
    pub sale_date: DateTime<Utc>,
    /// Null for a sale without a customer.
    pub customer_id: Option<i32>,
    /// The customer's name.
    pub customer_name: Option<String>,
    /// The sales rep who sold it.
    pub seller_name: Option<String>,
    /// Stone colors of its slabs, by name.
    pub stones: Vec<String>,
    /// Sinks sold with it, by model.
    pub sinks: Vec<SinkCount>,
    /// The price list of the deal's quote (the signed one, else the newest);
    /// null when the sale's deal has no quote.
    pub package: Option<String>,
    /// Sale amount, decimal string in USD.
    pub price: Option<String>,
    /// The deposit its sales order asks for: half of a price over $1,000,
    /// else all of it. Decimal string in USD.
    pub deposit: Option<String>,
    /// The customer's signed contract (the one of the sale's deal first,
    /// else the newest); null when nothing was signed.
    pub signed_order: Option<SignedOrder>,
}

/// Sinks of one model on a sale.
#[derive(Debug, Serialize, ToSchema)]
pub struct SinkCount {
    /// The sink model.
    pub name: String,
    /// How many of it were sold.
    pub count: i64,
}

/// A signed contract.
#[derive(Debug, Serialize, ToSchema)]
pub struct SignedOrder {
    /// The file's name.
    pub filename: String,
    /// When it was signed.
    #[serde(serialize_with = "js_date_required")]
    pub signed_at: DateTime<Utc>,
    /// Where to open it: an API path (`/v1/...`) to open on the API's host,
    /// else the file's URL.
    pub view_url: String,
}

#[derive(sqlx::FromRow)]
struct SaleRow {
    id: i32,
    sale_date: DateTime<Utc>,
    customer_id: Option<i32>,
    customer_name: Option<String>,
    seller_name: Option<String>,
    deal_id: Option<i64>,
    price: Option<String>,
    deposit: Option<String>,
}

#[derive(sqlx::FromRow)]
struct NameRow {
    sale_id: i32,
    name: String,
    count: i64,
}

#[derive(sqlx::FromRow)]
struct PackageRow {
    deal_id: i64,
    name: String,
}

#[derive(sqlx::FromRow)]
struct SignedRow {
    id: i64,
    customer_id: i32,
    deal_id: Option<i64>,
    filename: String,
    file_url: String,
    provider: String,
    provider_document_id: Option<String>,
    signed_at: DateTime<Utc>,
}

/// List a day's sales
///
/// Returns the company's sales from `from` up to `to` (not cancelled),
/// newest first: customer, sales rep, stone colors, sinks, package, amount,
/// deposit and the signed contract.
#[crate::openapi::path(
    get,
    path = "/v1/sales/daily",
    operation_id = "list_daily_sales",
    tag = "sales",
    params(DailySalesQuery),
    responses(
        (status = 200, description = "Sales, newest first", body = Vec<DailySale>),
        (status = 400, description = "`to` is not after `from`, or more than 31 days after it", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn daily(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<DailySalesQuery>,
) -> ApiResult<Json<Vec<DailySale>>> {
    if query.to <= query.from || query.to - query.from > Duration::days(MAX_DAYS) {
        return Err(ApiError::BadRequest(format!(
            "`to` must be after `from`, at most {MAX_DAYS} days"
        )));
    }
    let company_id = user.company_id;
    let pool = &state.pool;
    let sales: Vec<SaleRow> = sqlx::query_as(
        "SELECT s.id, s.sale_date, s.customer_id, c.name AS customer_name, u.name AS seller_name,
                CAST(s.deal_id AS SIGNED) AS deal_id, CAST(s.price AS CHAR) AS price,
                CAST(CAST(CASE WHEN s.price > ? THEN s.price / 2 ELSE s.price END
                          AS DECIMAL(14, 2)) AS CHAR) AS deposit
           FROM sales s
           LEFT JOIN customers c ON c.id = s.customer_id AND c.company_id = s.company_id
           LEFT JOIN users u ON u.id = s.seller_id
          WHERE s.company_id = ? AND s.cancelled_date IS NULL
            AND s.sale_date >= ? AND s.sale_date < ?
          ORDER BY s.sale_date DESC, s.id DESC",
    )
    .bind(HALF_DEPOSIT_OVER)
    .bind(company_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool)
    .await?;
    if sales.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let sale_ids: Vec<i32> = sales.iter().map(|sale| sale.id).collect();
    let stones = names_by_sale(
        pool,
        "SELECT si.sale_id, st.name, COUNT(*) AS count
           FROM slab_inventory si
           JOIN stones st ON st.id = si.stone_id
          WHERE si.deleted_at IS NULL AND st.name IS NOT NULL AND si.sale_id IN (",
        &sale_ids,
    )
    .await?;
    let sinks = names_by_sale(
        pool,
        "SELECT si.sale_id, t.name, COUNT(*) AS count
           FROM sinks k
           JOIN slab_inventory si ON si.id = k.slab_id
           JOIN sink_type t ON t.id = k.sink_type_id
          WHERE k.is_deleted = 0 AND t.name IS NOT NULL AND si.sale_id IN (",
        &sale_ids,
    )
    .await?;
    let deal_ids: Vec<i64> = sales.iter().filter_map(|sale| sale.deal_id).collect();
    let packages = packages(pool, company_id, &deal_ids).await?;
    let customer_ids: Vec<i32> = sales.iter().filter_map(|sale| sale.customer_id).collect();
    let signed = signed_contracts(pool, company_id, &customer_ids).await?;

    let mut out = Vec::with_capacity(sales.len());
    for sale in sales {
        let contract = signed
            .iter()
            .filter(|row| Some(row.customer_id) == sale.customer_id)
            .max_by_key(|row| {
                (
                    sale.deal_id.is_some() && row.deal_id == sale.deal_id,
                    row.signed_at,
                )
            });
        let signed_order = match contract {
            Some(row) => Some(SignedOrder {
                filename: row.filename.clone(),
                signed_at: row.signed_at,
                view_url: view_url(
                    &state,
                    SignedFile {
                        id: row.id,
                        customer_id: row.customer_id,
                        provider: &row.provider,
                        status: "signed",
                        provider_document_id: row.provider_document_id.as_deref(),
                        file_url: &row.file_url,
                    },
                )
                .await,
            }),
            None => None,
        };
        out.push(DailySale {
            id: sale.id,
            sale_date: sale.sale_date,
            customer_id: sale.customer_id,
            customer_name: sale.customer_name,
            seller_name: sale.seller_name,
            stones: stones
                .get(&sale.id)
                .map(|rows| rows.iter().map(|row| row.name.clone()).collect())
                .unwrap_or_default(),
            sinks: sinks
                .get(&sale.id)
                .map(|rows| {
                    rows.iter()
                        .map(|row| SinkCount {
                            name: row.name.clone(),
                            count: row.count,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            package: sale.deal_id.and_then(|id| packages.get(&id).cloned()),
            price: sale.price,
            deposit: sale.deposit,
            signed_order,
        });
    }
    Ok(Json(out))
}

/// Rows of `select` (ending in `IN (`) for `sale_ids`, grouped by sale and
/// name, each sale's names in order.
async fn names_by_sale(
    pool: &MySqlPool,
    select: &str,
    sale_ids: &[i32],
) -> Result<HashMap<i32, Vec<NameRow>>, sqlx::Error> {
    let mut sql = QueryBuilder::<MySql>::new(select);
    let mut ids = sql.separated(", ");
    for id in sale_ids {
        ids.push_bind(*id);
    }
    sql.push(") GROUP BY si.sale_id, 2 ORDER BY 2");
    let rows: Vec<NameRow> = sql.build_query_as().fetch_all(pool).await?;
    let mut by_sale: HashMap<i32, Vec<NameRow>> = HashMap::new();
    for row in rows {
        by_sale.entry(row.sale_id).or_default().push(row);
    }
    Ok(by_sale)
}

/// Each deal's package: the price list of its signed quote, else its newest.
async fn packages(
    pool: &MySqlPool,
    company_id: i32,
    deal_ids: &[i64],
) -> Result<HashMap<i64, String>, sqlx::Error> {
    if deal_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT CAST(q.deal_id AS SIGNED) AS deal_id, pl.name
           FROM quotes q
           JOIN price_lists pl ON pl.id = q.price_list_id
          WHERE q.deleted_at IS NULL AND q.company_id = ",
    );
    sql.push_bind(company_id).push(" AND q.deal_id IN (");
    let mut ids = sql.separated(", ");
    for id in deal_ids {
        ids.push_bind(*id);
    }
    // Best last, so it wins the map.
    sql.push(") ORDER BY q.signed_at IS NOT NULL, q.id");
    let rows: Vec<PackageRow> = sql.build_query_as().fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.deal_id, row.name))
        .collect())
}

/// Signed contracts of the customers. Nothing changes a request after it is
/// signed, so its last change is the signing.
async fn signed_contracts(
    pool: &MySqlPool,
    company_id: i32,
    customer_ids: &[i32],
) -> Result<Vec<SignedRow>, sqlx::Error> {
    if customer_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT CAST(sr.id AS SIGNED) AS id, sr.customer_id,
                CAST(COALESCE(sr.deal_id, e.deal_id) AS SIGNED) AS deal_id,
                sr.filename, sr.file_url, sr.provider, sr.provider_document_id,
                sr.updated_at AS signed_at
           FROM signature_requests sr
           LEFT JOIN emails e ON e.id = sr.email_id
          WHERE sr.status = 'signed' AND sr.company_id = ",
    );
    sql.push_bind(company_id).push(" AND sr.customer_id IN (");
    let mut ids = sql.separated(", ");
    for id in customer_ids {
        ids.push_bind(*id);
    }
    sql.push(")");
    sql.build_query_as().fetch_all(pool).await
}
