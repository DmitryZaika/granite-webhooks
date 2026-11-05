//! Whether a project is paid, from its customer's `QuickBooks` invoices.
//!
//! A project counts the invoices of its customer's `QuickBooks` customer
//! dated from 30 days before it started (a deposit invoice often comes just
//! before the win). A customer not linked to `QuickBooks` has no invoice, as
//! on Commissions.

use super::access::Viewer;
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::{IntoParams, ToSchema};
use crate::qbo::{QboError, escape};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, MySql, QueryBuilder};

/// `QuickBooks` customers per invoice query.
const CHUNK: usize = 50;

/// Query of `GET /v1/projects/payments`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PaymentsQuery {
    /// Project ids (`ids=1&ids=2`), at most 200.
    #[serde(default)]
    pub ids: Vec<i32>,
}

/// How much of a project's invoices is paid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PaymentStatus {
    /// No invoice yet.
    None,
    /// Nothing paid.
    Unpaid,
    /// Some paid, a balance left.
    Partial,
    /// Every invoice paid.
    Paid,
}

/// One project's payment.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectPayment {
    pub project_id: i32,
    pub status: PaymentStatus,
    /// What is still owed.
    pub balance: f64,
    /// The invoices' total.
    pub total: f64,
}

/// `GET /v1/projects/payments`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectPayments {
    /// False when `QuickBooks` is not connected or did not answer: then no
    /// project has an answer.
    pub connected: bool,
    /// The asked projects the caller sees whose customer is in `QuickBooks`.
    pub payments: Vec<ProjectPayment>,
}

#[derive(FromRow)]
struct Row {
    id: i32,
    qbo_id: Option<String>,
    /// `YYYY-MM-DD`: invoices from this day count.
    since: String,
}

/// One invoice of a `QuickBooks` query.
struct Invoice {
    customer: String,
    date: String,
    total: f64,
    balance: f64,
}

fn invoice(value: &Value) -> Option<Invoice> {
    Some(Invoice {
        customer: value["CustomerRef"]["value"].as_str()?.to_string(),
        date: value["TxnDate"].as_str().unwrap_or_default().to_string(),
        total: value["TotalAmt"].as_f64().unwrap_or_default(),
        balance: value["Balance"].as_f64().unwrap_or_default(),
    })
}

/// Paid, partly paid or not, from the invoices' total and what is left.
fn status(count: usize, total: f64, balance: f64) -> PaymentStatus {
    if count == 0 {
        PaymentStatus::None
    } else if balance <= 0.005 {
        PaymentStatus::Paid
    } else if balance < total - 0.005 {
        PaymentStatus::Partial
    } else {
        PaymentStatus::Unpaid
    }
}

/// The payments of `rows` from their customers' invoices.
fn payments(rows: &[Row], invoices: &[Invoice]) -> Vec<ProjectPayment> {
    rows.iter()
        .map(|row| {
            let mine: Vec<&Invoice> = invoices
                .iter()
                .filter(|invoice| {
                    row.qbo_id.as_deref() == Some(invoice.customer.as_str())
                        && invoice.date >= row.since
                })
                .collect();
            let total: f64 = mine.iter().map(|invoice| invoice.total).sum();
            let balance: f64 = mine.iter().map(|invoice| invoice.balance).sum();
            ProjectPayment {
                project_id: row.id,
                status: status(mine.len(), total, balance),
                balance,
                total,
            }
        })
        .collect()
}

/// Projects' payments
///
/// Whether each asked project is paid, from its customer's `QuickBooks`
/// invoices. Projects the caller doesn't see are left out; with `QuickBooks`
/// not connected, all are.
#[crate::openapi::path(
    get,
    path = "/v1/projects/payments",
    operation_id = "list_project_payments",
    tag = "projects",
    params(PaymentsQuery),
    responses(
        (status = 200, description = "Payments", body = ProjectPayments),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "Projects is off for the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<PaymentsQuery>,
) -> ApiResult<Json<ProjectPayments>> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let ids: Vec<i32> = query.ids.into_iter().take(200).collect();
    if ids.is_empty() {
        return Ok(Json(ProjectPayments {
            connected: true,
            payments: Vec::new(),
        }));
    }
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT p.id, NULLIF(TRIM(CAST(c.qbo_id AS CHAR)), '') AS qbo_id,
                DATE_FORMAT(p.created_at - INTERVAL 30 DAY, '%Y-%m-%d') AS since
           FROM projects p
           JOIN customers c ON c.id = p.customer_id
          WHERE p.company_id = ",
    );
    sql.push_bind(viewer.company_id)
        .push(format!(" AND {}", viewer.visible_sql()));
    crate::sql::push_in(&mut sql, "p.id", &ids);
    let rows: Vec<Row> = sql.build_query_as().fetch_all(&state.pool).await?;

    let mut customers: Vec<&str> = rows
        .iter()
        .filter_map(|row| row.qbo_id.as_deref())
        .collect();
    customers.sort_unstable();
    customers.dedup();
    let found = async {
        let access = state.qbo.access(&state.pool, viewer.company_id).await?;
        let mut invoices = Vec::new();
        for chunk in customers.chunks(CHUNK) {
            let list: Vec<String> = chunk
                .iter()
                .map(|id| format!("'{}'", escape(id)))
                .collect();
            let rows = state
                .qbo
                .query(
                    &access,
                    &format!(
                        "select Id, CustomerRef, TxnDate, TotalAmt, Balance from Invoice where CustomerRef in ({}) MAXRESULTS 1000",
                        list.join(", ")
                    ),
                    "Invoice",
                )
                .await?;
            invoices.extend(rows.iter().filter_map(invoice));
        }
        Ok::<_, QboError>(invoices)
    }
    .await;
    let invoices = match found {
        Ok(invoices) => invoices,
        Err(QboError::NotConnected | QboError::Reconnect) => {
            return Ok(Json(ProjectPayments {
                connected: false,
                payments: Vec::new(),
            }));
        }
        Err(error) => {
            tracing::error!(?error, "could not read the projects' QuickBooks invoices");
            return Ok(Json(ProjectPayments {
                connected: false,
                payments: Vec::new(),
            }));
        }
    };
    Ok(Json(ProjectPayments {
        connected: true,
        payments: payments(&rows, &invoices),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bill(customer: &str, date: &str, total: f64, balance: f64) -> Invoice {
        Invoice {
            customer: customer.into(),
            date: date.into(),
            total,
            balance,
        }
    }

    fn row(id: i32, qbo_id: Option<&str>) -> Row {
        Row {
            id,
            qbo_id: qbo_id.map(Into::into),
            since: "2026-10-01".into(),
        }
    }

    #[test]
    fn paid_partly_paid_or_not() {
        let invoices = [
            bill("1", "2026-10-05", 1000.0, 0.0),
            bill("2", "2026-10-05", 1000.0, 400.0),
            bill("3", "2026-10-05", 500.0, 500.0),
            // Before the project: an older job's.
            bill("4", "2026-01-05", 900.0, 0.0),
        ];
        let rows = [
            row(10, Some("1")),
            row(11, Some("2")),
            row(12, Some("3")),
            row(13, Some("4")),
            row(14, None),
        ];
        let found: Vec<(i32, PaymentStatus)> = payments(&rows, &invoices)
            .iter()
            .map(|p| (p.project_id, p.status))
            .collect();
        assert_eq!(
            found,
            [
                (10, PaymentStatus::Paid),
                (11, PaymentStatus::Partial),
                (12, PaymentStatus::Unpaid),
                (13, PaymentStatus::None),
                (14, PaymentStatus::None),
            ]
        );
    }
}
