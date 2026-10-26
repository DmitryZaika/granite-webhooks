//! `GET /v1/qbo/class-sales`: the `QuickBooks` "Sales by Class" report.
//!
//! The Statistics page parses the report (`qboClassSales.ts`), so it comes
//! back as `QuickBooks` sends it, with the company's classes.

use crate::auth::AdminUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::{IntoParams, ToSchema};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Query of `GET /v1/qbo/class-sales`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ClassSalesQuery {
    /// First day (`YYYY-MM-DD`); `QuickBooks`' default when empty.
    #[serde(default)]
    pub start_date: String,
    /// Last day (`YYYY-MM-DD`); `QuickBooks`' default when empty.
    #[serde(default)]
    pub end_date: String,
}

/// `ok`, `not_connected` or `error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClassSalesStatus {
    Ok,
    NotConnected,
    Error,
}

/// `GET /v1/qbo/class-sales`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ClassSales {
    /// Whether the report came back.
    pub status: ClassSalesStatus,
    /// The `ClassSales` report as `QuickBooks` returns it (with `ok`).
    #[schema(value_type = Option<Object>)]
    pub report: Option<Value>,
    /// The company's `Class` entities (empty when they could not be read).
    #[schema(value_type = Vec<Object>)]
    pub classes: Vec<Value>,
}

fn day(value: &str) -> Option<&str> {
    let value = value.trim();
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .map(|_| value)
}

/// Sales by class
///
/// The company's `QuickBooks` "Sales by Class" report (accrual basis) for the
/// days, and its classes so the page can nest them. `not_connected` when
/// `QuickBooks` is off or disconnected, `error` when the report failed.
#[crate::openapi::path(
    get,
    path = "/v1/qbo/class-sales",
    operation_id = "get_qbo_class_sales",
    tag = "quickbooks",
    params(ClassSalesQuery),
    responses(
        (status = 200, description = "The report, or why there is none", body = ClassSales),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn class_sales(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    MultiQuery(query): MultiQuery<ClassSalesQuery>,
) -> ApiResult<Json<ClassSales>> {
    let empty = |status| {
        Json(ClassSales {
            status,
            report: None,
            classes: Vec::new(),
        })
    };
    let access = match state.qbo.access(&state.pool, user.company_id).await {
        Ok(access) => access,
        Err(err) if err.is_not_connected() => return Ok(empty(ClassSalesStatus::NotConnected)),
        Err(err) => {
            tracing::warn!(error = %err, "QuickBooks access failed");
            return Ok(empty(ClassSalesStatus::Error));
        }
    };
    let mut path = String::from("reports/ClassSales?accounting_method=Accrual");
    if let Some(start) = day(&query.start_date) {
        path.push_str("&start_date=");
        path.push_str(start);
    }
    if let Some(end) = day(&query.end_date) {
        path.push_str("&end_date=");
        path.push_str(end);
    }
    // The report and the class list don't depend on each other.
    let (report, classes) = tokio::join!(
        state.qbo.get(&access, &path),
        state
            .qbo
            .query(&access, "select * from Class MAXRESULTS 1000", "Class"),
    );
    let report = match report {
        Ok(report) if report.get("Fault").is_none() => report,
        Ok(_) => return Ok(empty(ClassSalesStatus::Error)),
        Err(err) if err.is_not_connected() => return Ok(empty(ClassSalesStatus::NotConnected)),
        Err(err) => {
            tracing::warn!(error = %err, "QuickBooks class sales report failed");
            return Ok(empty(ClassSalesStatus::Error));
        }
    };
    let classes = classes.unwrap_or_default();
    Ok(Json(ClassSales {
        status: ClassSalesStatus::Ok,
        report: Some(report),
        classes,
    }))
}
