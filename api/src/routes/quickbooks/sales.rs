//! The signed-in rep's `QuickBooks` sales this month: the total of their
//! classes in the Sales by Class report, and the customers behind it.
//!
//! Port of `qboSalesRepAmount.server.ts`, `qboClassSales.ts` and
//! `qboCustomerSales.ts`. A rep's classes are the classes named after them
//! (`Rita Rep`, `Rita/...`, or their first name), with their subclasses.

use super::invoice::{number, text};
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::qbo::{Access, QboError, escape};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{Datelike, Duration, NaiveDate};
use chrono_tz::America::Indiana::Indianapolis;
use lambda_http::tracing;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

/// A data row of a `QuickBooks` summary report.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportRow {
    pub name: String,
    pub amount: Option<f64>,
    /// The customer (or class) the row is for.
    pub entity_id: Option<String>,
}

/// A summary report's data rows and grand total.
#[derive(Debug, Default)]
pub struct Report {
    pub rows: Vec<ReportRow>,
    pub total: Option<f64>,
    pub start_period: Option<String>,
    pub end_period: Option<String>,
    pub currency: Option<String>,
}

fn list(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

fn trimmed(value: &Value) -> Option<String> {
    text(value)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn amount(value: &Value) -> Option<f64> {
    match value {
        Value::String(text) => text
            .replace(',', "")
            .trim()
            .parse()
            .ok()
            .filter(|n: &f64| n.is_finite()),
        other => number(other),
    }
}

/// Name, amount and entity id of a `ColData` holder.
fn named_amount(holder: &Value) -> Option<ReportRow> {
    let cols = list(&holder["ColData"]);
    let name_col = cols.first()?;
    Some(ReportRow {
        name: trimmed(&name_col["value"])?,
        amount: cols.get(1).and_then(|col| amount(&col["value"])),
        entity_id: trimmed(&name_col["id"]),
    })
}

fn collect(rows: &Value, report: &mut Report) {
    for row in list(rows) {
        if !row.is_object() {
            continue;
        }
        if row["group"] == "GrandTotal" {
            if let Some(total) = named_amount(&row["Summary"]) {
                report.total = total.amount;
            }
            continue;
        }
        if row["Rows"].get("Row").is_some() {
            collect(&row["Rows"]["Row"], report);
        } else if row.get("ColData").is_some()
            && let Some(data) = named_amount(&row)
        {
            report.rows.push(data);
        }
    }
}

/// The CRM's `parseQboClassSalesReport`, keeping the data rows.
pub fn parse_report(payload: &Value) -> Report {
    let header = &payload["Header"];
    let mut report = Report {
        start_period: trimmed(&header["StartPeriod"]),
        end_period: trimmed(&header["EndPeriod"]),
        currency: trimmed(&header["Currency"]),
        ..Report::default()
    };
    collect(&payload["Rows"]["Row"], &mut report);
    report
}

/// The CRM's `rowMatchesSalesRepName`: the rep's full name, `name/...`, or
/// a first name of three letters or more.
pub fn matches_rep(row_name: &str, rep_name: &str) -> bool {
    let row = row_name.trim().to_lowercase();
    let rep = rep_name.trim().to_lowercase();
    if row.is_empty() || rep.is_empty() {
        return false;
    }
    if row == rep || row.starts_with(&format!("{rep}/")) {
        return true;
    }
    let first = rep.split_whitespace().next().unwrap_or("");
    first.chars().count() >= 3 && (row == first || row.starts_with(&format!("{first}/")))
}

/// A `QuickBooks` class.
#[derive(Debug, Clone)]
pub struct Class {
    pub id: String,
    pub name: String,
    pub qualified_name: String,
    pub parent_id: Option<String>,
}

/// Classes with parents filled in from `Parent:Child` names when
/// `QuickBooks` left `ParentRef` out.
pub fn resolve_parents(mut classes: Vec<Class>) -> Vec<Class> {
    let by_name: Vec<(String, String)> = classes
        .iter()
        .map(|class| (class.qualified_name.trim().to_lowercase(), class.id.clone()))
        .collect();
    for class in &mut classes {
        if class.parent_id.is_some() {
            continue;
        }
        let Some(separator) = class.qualified_name.rfind(':').filter(|at| *at > 0) else {
            continue;
        };
        let parent = class.qualified_name[..separator].trim().to_lowercase();
        class.parent_id = by_name
            .iter()
            .find(|(name, id)| *name == parent && *id != class.id)
            .map(|(_, id)| id.clone());
    }
    classes
}

/// The rep's classes and all their subclasses.
pub fn rep_class_ids(classes: &[Class], rep_name: &str) -> Vec<String> {
    let mut matched: Vec<String> = classes
        .iter()
        .filter(|class| {
            let leaf = class.qualified_name.rsplit(':').next().unwrap_or("");
            matches_rep(&class.name, rep_name)
                || matches_rep(&class.qualified_name, rep_name)
                || matches_rep(leaf, rep_name)
        })
        .map(|class| class.id.clone())
        .collect();
    let mut seen: HashSet<String> = matched.iter().cloned().collect();
    loop {
        let added: Vec<String> = classes
            .iter()
            .filter(|class| {
                class
                    .parent_id
                    .as_ref()
                    .is_some_and(|parent| seen.contains(parent))
                    && !seen.contains(&class.id)
            })
            .map(|class| class.id.clone())
            .collect();
        if added.is_empty() {
            return matched;
        }
        seen.extend(added.iter().cloned());
        matched.extend(added);
    }
}

/// This month so far, in Indianapolis time.
#[derive(Debug, Clone, Copy)]
pub struct Periods {
    pub month_start: NaiveDate,
    pub today: NaiveDate,
    pub month_end: NaiveDate,
    pub week_end: NaiveDate,
    pub days_left_in_month: u32,
    pub days_left_in_week: u32,
}

impl Periods {
    /// The CRM's `monthToDateIsoRange` and `weekToDateIsoRange` for `today`.
    pub fn of(today: NaiveDate) -> Self {
        let month_start = today.with_day(1).unwrap_or(today);
        let next_month = if today.month() == 12 {
            NaiveDate::from_ymd_opt(today.year() + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(today.year(), today.month() + 1, 1)
        };
        let month_end = next_month.map_or(today, |next| next - Duration::days(1));
        let since_monday = today.weekday().num_days_from_monday();
        Self {
            month_start,
            today,
            month_end,
            week_end: today + Duration::days(i64::from(6 - since_monday)),
            days_left_in_month: month_end.day() - today.day() + 1,
            days_left_in_week: 7 - since_monday,
        }
    }

    fn now() -> Self {
        Self::of(chrono::Utc::now().with_timezone(&Indianapolis).date_naive())
    }
}

async fn report(
    state: &AppState,
    access: &Access,
    name: &str,
    query: &str,
) -> Result<Report, QboError> {
    let payload = state
        .qbo
        .get(access, &format!("reports/{name}?{query}"))
        .await?;
    if payload.get("Fault").is_some() {
        return Err(QboError::Request(format!("{name} report failed")));
    }
    Ok(parse_report(&payload))
}

async fn classes(state: &AppState, access: &Access) -> Result<Vec<Class>, QboError> {
    let rows = state
        .qbo
        .query(access, "select * from Class MAXRESULTS 1000", "Class")
        .await?;
    let classes = rows
        .iter()
        .filter_map(|row| {
            let name = text(&row["Name"])?;
            Some(Class {
                id: text(&row["Id"])?,
                qualified_name: text(&row["FullyQualifiedName"]).unwrap_or_else(|| name.clone()),
                name,
                parent_id: text(&row["ParentRef"]["value"]),
            })
        })
        .collect();
    Ok(resolve_parents(classes))
}

async fn rep_name(state: &AppState, user_id: i32) -> Result<String, sqlx::Error> {
    let name: Option<Option<String>> =
        sqlx::query_scalar("SELECT name FROM users WHERE id = ? AND is_deleted = 0")
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await?;
    Ok(name.flatten().unwrap_or_default().trim().to_string())
}

/// `ok`, `error`, or `unavailable` when the company does not use
/// `QuickBooks` (or the rep has no name to match).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SalesStatus {
    Ok,
    Error,
    Unavailable,
}

/// `GET /v1/me/qbo-sales`.
#[derive(Debug, Serialize, ToSchema)]
pub struct RepSales {
    pub status: SalesStatus,
    /// The rep's classes' sales this month.
    pub amount: Option<f64>,
    /// Currency code.
    pub currency: Option<String>,
    /// First day of the report.
    pub start_period: Option<String>,
    /// Last day of the report (today).
    pub end_period: Option<String>,
    /// Last day of the month.
    pub month_end_period: Option<String>,
    /// Sunday of this week.
    pub week_end_period: Option<String>,
    /// Days left in the week, today included.
    pub days_remaining_in_week: Option<u32>,
    /// Days left in the month, today included.
    pub days_remaining_in_month: Option<u32>,
}

impl RepSales {
    const fn only(status: SalesStatus) -> Self {
        Self {
            status,
            amount: None,
            currency: None,
            start_period: None,
            end_period: None,
            month_end_period: None,
            week_end_period: None,
            days_remaining_in_week: None,
            days_remaining_in_month: None,
        }
    }
}

fn currency(report: &Report) -> String {
    report
        .currency
        .clone()
        .filter(|code| !code.is_empty())
        .unwrap_or_else(|| "USD".into())
}

/// Get my QuickBooks sales this month
///
/// The sum of the caller's classes in this month's Sales by Class report,
/// with the periods the sales goal panel shows.
#[crate::openapi::path(
    get,
    path = "/v1/me/qbo-sales",
    operation_id = "get_my_qbo_sales",
    tag = "quickbooks",
    responses(
        (status = 200, description = "Sales", body = RepSales),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn my_sales(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<RepSales>> {
    let name = rep_name(&state, user.id).await?;
    let access = match state.qbo.access(&state.pool, user.company_id).await {
        Ok(_) if name.is_empty() => return Ok(Json(RepSales::only(SalesStatus::Unavailable))),
        Ok(access) => access,
        Err(QboError::NotConnected) => return Ok(Json(RepSales::only(SalesStatus::Unavailable))),
        Err(QboError::Database(err)) => return Err(err.into()),
        Err(err) => {
            tracing::warn!(error = %err, "QuickBooks sales unavailable");
            return Ok(Json(RepSales::only(SalesStatus::Error)));
        }
    };
    let periods = Periods::now();
    let query = format!(
        "accounting_method=Accrual&start_date={}&end_date={}",
        periods.month_start, periods.today
    );
    let report = match report(&state, &access, "ClassSales", &query).await {
        Ok(report) => report,
        Err(err) => {
            tracing::warn!(error = %err, "QuickBooks class sales failed");
            return Ok(Json(RepSales::only(SalesStatus::Error)));
        }
    };
    let amount = report
        .rows
        .iter()
        .filter(|row| matches_rep(&row.name, &name))
        .filter_map(|row| row.amount)
        .sum();
    Ok(Json(RepSales {
        status: SalesStatus::Ok,
        amount: Some(amount),
        currency: Some(currency(&report)),
        start_period: Some(
            report
                .start_period
                .clone()
                .unwrap_or_else(|| periods.month_start.to_string()),
        ),
        end_period: Some(
            report
                .end_period
                .clone()
                .unwrap_or_else(|| periods.today.to_string()),
        ),
        month_end_period: Some(periods.month_end.to_string()),
        week_end_period: Some(periods.week_end.to_string()),
        days_remaining_in_week: Some(periods.days_left_in_week),
        days_remaining_in_month: Some(periods.days_left_in_month),
    }))
}

/// An invoice behind a customer's sales.
#[derive(Debug, Serialize, ToSchema)]
pub struct SalesInvoice {
    /// `QuickBooks` invoice id.
    pub id: String,
    /// Invoice number.
    pub doc_number: Option<String>,
    /// Total.
    pub total_amt: Option<f64>,
    /// Invoice date.
    pub txn_date: Option<String>,
}

/// A customer's sales to the rep this month.
#[derive(Debug, Serialize, ToSchema)]
pub struct CustomerSales {
    /// Customer name as `QuickBooks` has it.
    pub name: String,
    /// Sales this month.
    pub amount: f64,
    /// `QuickBooks` customer id.
    pub qbo_customer_id: Option<String>,
    /// This month's invoices, oldest first.
    pub invoices: Vec<SalesInvoice>,
}

/// `ok`, `error` or `not_connected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CustomersStatus {
    Ok,
    Error,
    NotConnected,
}

/// `GET /v1/me/qbo-sales/customers`.
#[derive(Debug, Serialize, ToSchema)]
pub struct RepCustomers {
    pub status: CustomersStatus,
    /// Customers, by their first invoice date.
    pub customers: Vec<CustomerSales>,
    /// Sum of the customers' sales.
    pub total: f64,
    /// Currency code.
    pub currency: String,
}

impl RepCustomers {
    fn empty(status: CustomersStatus) -> Self {
        Self {
            status,
            customers: Vec::new(),
            total: 0.0,
            currency: "USD".into(),
        }
    }
}

struct DatedInvoice {
    invoice: SalesInvoice,
    customer_id: Option<String>,
    customer_name: Option<String>,
}

async fn month_invoices(
    state: &AppState,
    access: &Access,
    periods: &Periods,
) -> Result<Vec<DatedInvoice>, QboError> {
    let rows = state
        .qbo
        .query(
            access,
            &format!(
                "select Id, DocNumber, TxnDate, TotalAmt, CustomerRef from Invoice where TxnDate >= '{}' and TxnDate <= '{}' ORDERBY TxnDate MAXRESULTS 1000",
                escape(&periods.month_start.to_string()),
                escape(&periods.today.to_string())
            ),
            "Invoice",
        )
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some(DatedInvoice {
                invoice: SalesInvoice {
                    id: text(&row["Id"])?,
                    doc_number: text(&row["DocNumber"]),
                    total_amt: number(&row["TotalAmt"]),
                    txn_date: text(&row["TxnDate"]),
                },
                customer_id: text(&row["CustomerRef"]["value"]),
                customer_name: text(&row["CustomerRef"]["name"]),
            })
        })
        .collect())
}

/// Natural order of invoice numbers: `INV-9` before `INV-10`.
fn natural_key(text: &str) -> Vec<(String, u64)> {
    let mut parts = Vec::new();
    let mut letters = String::new();
    let mut digits = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            if !digits.is_empty() {
                parts.push((
                    std::mem::take(&mut letters),
                    digits.parse().unwrap_or(u64::MAX),
                ));
                digits.clear();
            }
            letters.push(ch);
        }
    }
    parts.push((letters, digits.parse().unwrap_or(0)));
    parts
}

/// Empty dates sort last.
fn by_date(left: Option<&String>, right: Option<&String>) -> std::cmp::Ordering {
    match (
        left.filter(|d| !d.is_empty()),
        right.filter(|d| !d.is_empty()),
    ) {
        (Some(l), Some(r)) => l.cmp(r),
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// The CRM's `customerSalesRowsFromReport` + `attachInvoicesToCustomerSales`.
fn customer_rows(report: &Report, invoices: &[DatedInvoice]) -> Vec<CustomerSales> {
    let mut customers: Vec<CustomerSales> = report
        .rows
        .iter()
        .filter_map(|row| {
            let amount = row.amount.filter(|amount| *amount != 0.0)?;
            let invoices = invoices
                .iter()
                .filter(|dated| {
                    (row.entity_id.is_some() && dated.customer_id == row.entity_id)
                        || dated
                            .customer_name
                            .as_deref()
                            .is_some_and(|name| name.trim().eq_ignore_ascii_case(row.name.trim()))
                })
                .map(|dated| SalesInvoice {
                    id: dated.invoice.id.clone(),
                    doc_number: dated.invoice.doc_number.clone(),
                    total_amt: dated.invoice.total_amt,
                    txn_date: dated.invoice.txn_date.clone(),
                })
                .collect();
            Some(CustomerSales {
                name: row.name.clone(),
                amount,
                qbo_customer_id: row.entity_id.clone(),
                invoices,
            })
        })
        .collect();
    for customer in &mut customers {
        customer.invoices.sort_by(|a, b| {
            by_date(a.txn_date.as_ref(), b.txn_date.as_ref()).then_with(|| {
                natural_key(a.doc_number.as_deref().unwrap_or(""))
                    .cmp(&natural_key(b.doc_number.as_deref().unwrap_or("")))
            })
        });
    }
    customers.sort_by(|a, b| {
        by_date(
            a.invoices.first().and_then(|i| i.txn_date.as_ref()),
            b.invoices.first().and_then(|i| i.txn_date.as_ref()),
        )
        .then_with(|| a.name.cmp(&b.name))
    });
    customers
}

/// List my QuickBooks customers this month
///
/// The customers in this month's Sales by Customer report for the caller's
/// classes, each with this month's invoices.
#[crate::openapi::path(
    get,
    path = "/v1/me/qbo-sales/customers",
    operation_id = "list_my_qbo_sales_customers",
    tag = "quickbooks",
    responses(
        (status = 200, description = "Customers", body = RepCustomers),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn my_customers(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<RepCustomers>> {
    let name = rep_name(&state, user.id).await?;
    let result = async {
        let access = state.qbo.access(&state.pool, user.company_id).await?;
        if name.is_empty() {
            return Ok(RepCustomers::empty(CustomersStatus::Ok));
        }
        let class_ids = rep_class_ids(&classes(&state, &access).await?, &name);
        if class_ids.is_empty() {
            return Ok(RepCustomers::empty(CustomersStatus::Ok));
        }
        let periods = Periods::now();
        let query = format!(
            "accounting_method=Accrual&summarize_column_by=Total&start_date={}&end_date={}&class={}",
            periods.month_start,
            periods.today,
            class_ids.join(",")
        );
        let report = report(&state, &access, "CustomerSales", &query).await?;
        let invoices = month_invoices(&state, &access, &periods).await.unwrap_or_default();
        let customers = customer_rows(&report, &invoices);
        Ok::<_, QboError>(RepCustomers {
            status: CustomersStatus::Ok,
            total: report
                .total
                .unwrap_or_else(|| customers.iter().map(|customer| customer.amount).sum()),
            currency: currency(&report),
            customers,
        })
    }
    .await;
    Ok(Json(match result {
        Ok(customers) => customers,
        Err(err) if err.is_not_connected() => RepCustomers::empty(CustomersStatus::NotConnected),
        Err(QboError::Database(err)) => return Err(err.into()),
        Err(err) => {
            tracing::warn!(error = %err, "QuickBooks customer sales failed");
            RepCustomers::empty(CustomersStatus::Error)
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn report_rows_and_rep_names() {
        let payload = json!({
            "Header": { "Currency": "USD", "StartPeriod": "2026-05-01" },
            "Rows": { "Row": [
                { "Header": { "ColData": [{ "value": "Sales" }] },
                  "Rows": { "Row": [
                      { "ColData": [{ "value": "Rita Rep" }, { "value": "1,200.50" }] },
                      { "ColData": [{ "value": "Rita/Remodel" }, { "value": "300" }] },
                      { "ColData": [{ "value": "Mark" }, { "value": "99" }] },
                  ] },
                  "Summary": { "ColData": [{ "value": "Total Sales" }, { "value": "1599.5" }] } },
                { "group": "GrandTotal", "Summary": { "ColData": [{ "value": "TOTAL" }, { "value": "1599.5" }] } },
            ] },
        });
        let report = parse_report(&payload);
        assert_eq!(report.rows.len(), 3);
        assert_eq!(report.total, Some(1599.5));
        let sum: f64 = report
            .rows
            .iter()
            .filter(|row| matches_rep(&row.name, "Rita Rep"))
            .filter_map(|row| row.amount)
            .sum();
        assert!((sum - 1500.5).abs() < 1e-9);
        assert!(!matches_rep("Ri", "Ri Lo"));
    }

    #[test]
    fn subclasses_follow_their_parent() {
        let class = |id: &str, name: &str, qualified: &str| Class {
            id: id.into(),
            name: name.into(),
            qualified_name: qualified.into(),
            parent_id: None,
        };
        let classes = resolve_parents(vec![
            class("1", "Rita", "Rita"),
            class("2", "Kitchens", "Rita:Kitchens"),
            class("3", "Mark", "Mark"),
        ]);
        assert_eq!(rep_class_ids(&classes, "Rita Rep"), ["1", "2"]);
    }

    #[test]
    fn periods_count_today() {
        let periods = Periods::of(NaiveDate::from_ymd_opt(2026, 2, 26).unwrap());
        assert_eq!(periods.month_end.to_string(), "2026-02-28");
        assert_eq!(periods.days_left_in_month, 3);
        assert_eq!(periods.week_end.to_string(), "2026-03-01");
        assert_eq!(periods.days_left_in_week, 4);
    }
}
