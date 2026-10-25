//! `QuickBooks` Online invoices and customers.
//!
//! - `GET /v1/customers/{id}/qbo-invoices` → the customer's invoices,
//!   linking the customer to its `QuickBooks` customer on first use
//! - `GET`/`PUT /v1/customers/{id}/qbo-invoices/{invoice_id}` → one
//!   invoice for the editor, and saving it
//! - `GET /v1/customers/{id}/qbo-invoices/{invoice_id}/pdf` → its PDF
//! - `GET /v1/qbo/invoices/{invoice_id}/pdf` → any invoice of the company
//!   as a PDF (the rep's sales panel, schedule events)
//! - `GET /v1/qbo/customers` → `QuickBooks` customers by email or name
//! - `GET /v1/qbo/class-sales` → the "Sales by Class" report (admins)
//! - `GET /v1/me/qbo-sales` → the rep's sales this month, and
//!   `/customers` the customers behind them
//!
//! Replaces the Remix `api.quickbooks.*` and `api.employee.qbo-*` routes.

pub mod class_sales;
pub mod invoice;
pub mod sales;

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::qbo::{Access, QboError, escape, valid_id};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use invoice::{EditableInvoice, InvoiceSummary, editable, summary, text, update_payload};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(customer_invoices))
        .routes(routes!(get_invoice, update_invoice))
        .routes(routes!(customer_invoice_pdf))
        .routes(routes!(invoice_pdf))
        .routes(routes!(customers))
        .routes(routes!(sales::my_sales))
        .routes(routes!(sales::my_customers))
        .routes(routes!(class_sales::class_sales))
}

/// What a `QuickBooks` failure means to the page.
#[allow(clippy::needless_pass_by_value)] // used as `map_err(qbo_error)`
fn qbo_error(err: QboError) -> ApiError {
    if err.is_not_connected() {
        return ApiError::Conflict("QuickBooks is not connected".into());
    }
    tracing::error!(error = %err, "QuickBooks request failed");
    ApiError::Upstream("QuickBooks request failed".into())
}

/// `ok`, or why there are no invoices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvoicesStatus {
    Ok,
    /// The company has not connected `QuickBooks`.
    NotConnected,
    /// No `QuickBooks` customer matches this contact.
    MissingQboId,
    /// `QuickBooks` failed.
    Error,
}

/// `GET /v1/customers/{id}/qbo-invoices`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CustomerInvoices {
    pub status: InvoicesStatus,
    /// Newest first.
    pub invoices: Vec<InvoiceSummary>,
    /// The `QuickBooks` customer id.
    pub qbo_id: Option<String>,
}

impl CustomerInvoices {
    const fn empty(status: InvoicesStatus) -> Self {
        Self {
            status,
            invoices: Vec::new(),
            qbo_id: None,
        }
    }
}

#[derive(sqlx::FromRow)]
struct CustomerRow {
    name: Option<String>,
    qbo_id: Option<String>,
    email: Option<String>,
}

async fn customer(state: &AppState, company_id: i32, customer_id: i32) -> ApiResult<CustomerRow> {
    let row: Option<CustomerRow> = sqlx::query_as(
        "SELECT c.name, NULLIF(TRIM(CAST(c.qbo_id AS CHAR)), '') AS qbo_id, ce.email
           FROM customers c
           LEFT JOIN customers_emails ce ON ce.id = c.email_id AND ce.customer_id = c.id
          WHERE c.id = ? AND c.company_id = ? AND c.deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    row.ok_or(ApiError::NotFound)
}

/// A `QuickBooks` customer.
#[derive(Debug, Serialize, ToSchema)]
pub struct QboCustomer {
    /// `QuickBooks` customer id.
    pub id: String,
    /// Display name.
    pub display_name: String,
    /// Primary email.
    pub email: Option<String>,
}

fn qbo_customer(row: &Value) -> Option<QboCustomer> {
    let id = text(&row["Id"])?;
    Some(QboCustomer {
        display_name: text(&row["DisplayName"])
            .unwrap_or_else(|| format!("QuickBooks customer {id}")),
        email: text(&row["PrimaryEmailAddr"]["Address"]),
        id,
    })
}

async fn by_email(state: &AppState, access: &Access, email: &str) -> Result<Vec<Value>, QboError> {
    state
        .qbo
        .query(
            access,
            &format!(
                "select * from Customer WHERE PrimaryEmailAddr = '{}'",
                escape(email)
            ),
            "Customer",
        )
        .await
}

async fn by_name(
    state: &AppState,
    access: &Access,
    name: &str,
) -> Result<Vec<QboCustomer>, QboError> {
    let rows = state
        .qbo
        .query(
            access,
            &format!(
                "select Id, DisplayName, PrimaryEmailAddr from Customer where DisplayName like '%{}%' maxresults 25",
                escape(name.trim())
            ),
            "Customer",
        )
        .await?;
    Ok(rows.iter().filter_map(qbo_customer).collect())
}

/// The CRM's `pickQboCustomerByName`: an exact name, then a unique email
/// match, then the only result.
fn pick_by_name(matches: &[QboCustomer], name: &str, email: &str) -> Option<String> {
    let key = |value: &str| value.trim().to_lowercase();
    let (name, email) = (key(name), key(email));
    let with_email = |list: Vec<&QboCustomer>| -> Option<String> {
        let found: Vec<_> = list
            .into_iter()
            .filter(|found| found.email.as_deref().is_some_and(|e| key(e) == email))
            .collect();
        (found.len() == 1).then(|| found[0].id.clone())
    };
    let exact: Vec<&QboCustomer> = matches
        .iter()
        .filter(|found| key(&found.display_name) == name)
        .collect();
    if exact.len() == 1 {
        return Some(exact[0].id.clone());
    }
    if !email.is_empty() {
        if exact.len() > 1
            && let Some(id) = with_email(exact)
        {
            return Some(id);
        }
        if let Some(id) = with_email(matches.iter().collect()) {
            return Some(id);
        }
    }
    (matches.len() == 1).then(|| matches[0].id.clone())
}

async fn resolve_qbo_id(
    state: &AppState,
    access: &Access,
    row: &CustomerRow,
) -> Result<Option<String>, QboError> {
    let email = row.email.as_deref().unwrap_or("").trim();
    let name = row.name.as_deref().unwrap_or("").trim();
    if !email.is_empty() {
        let matches = by_email(state, access, email).await?;
        if let [only] = matches.as_slice()
            && let Some(id) = text(&only["Id"])
        {
            return Ok(Some(id.trim().to_string()));
        }
    }
    if name.is_empty() {
        return Ok(None);
    }
    Ok(pick_by_name(
        &by_name(state, access, name).await?,
        name,
        email,
    ))
}

async fn invoices_for(
    state: &AppState,
    access: &Access,
    qbo_id: &str,
) -> Result<Vec<InvoiceSummary>, QboError> {
    let rows = state
        .qbo
        .query(
            access,
            &format!(
                "select Id, DocNumber, TxnDate, DueDate, TotalAmt, Balance, CurrencyRef, ShipAddr, MetaData from Invoice where CustomerRef = '{}' MAXRESULTS 1000",
                escape(qbo_id)
            ),
            "Invoice",
        )
        .await?;
    let mut invoices: Vec<InvoiceSummary> = rows.iter().filter_map(summary).collect();
    invoices.sort_by(|a, b| b.txn_date.cmp(&a.txn_date));
    Ok(invoices)
}

/// List a customer's QuickBooks invoices
///
/// The invoices of the customer's `QuickBooks` customer, newest first. A
/// customer not linked yet is matched by email, then by name, and linked.
/// `status` says why there are none when `QuickBooks` is not connected or
/// has no matching customer.
#[crate::openapi::path(
    get,
    path = "/v1/customers/{id}/qbo-invoices",
    operation_id = "list_customer_qbo_invoices",
    tag = "quickbooks",
    params(("id" = i32, Path, description = "Customer id")),
    responses(
        (status = 200, description = "Invoices", body = CustomerInvoices),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn customer_invoices(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(customer_id): Path<i32>,
) -> ApiResult<Json<CustomerInvoices>> {
    invoices_of_customer(&state, user.company_id, customer_id)
        .await
        .map(Json)
}

/// A customer's `QuickBooks` invoices, linking the customer first when it
/// is not linked yet (also for the customer's own view).
pub async fn invoices_of_customer(
    state: &AppState,
    company_id: i32,
    customer_id: i32,
) -> ApiResult<CustomerInvoices> {
    let row = customer(state, company_id, customer_id).await?;
    let result = async {
        let access = state.qbo.access(&state.pool, company_id).await?;
        let qbo_id = if let Some(id) = row.qbo_id.clone() {
            id
        } else {
            let Some(id) = resolve_qbo_id(state, &access, &row).await? else {
                return Ok(CustomerInvoices::empty(InvoicesStatus::MissingQboId));
            };
            sqlx::query("UPDATE customers SET qbo_id = ? WHERE id = ? AND company_id = ? AND deleted_at IS NULL")
                .bind(&id)
                .bind(customer_id)
                .bind(company_id)
                .execute(&state.pool)
                .await?;
            id
        };
        Ok::<_, QboError>(CustomerInvoices {
            status: InvoicesStatus::Ok,
            invoices: invoices_for(state, &access, &qbo_id).await?,
            qbo_id: Some(qbo_id),
        })
    }
    .await;
    Ok(match result {
        Ok(found) => found,
        Err(err) if err.is_not_connected() => CustomerInvoices::empty(InvoicesStatus::NotConnected),
        Err(QboError::Database(err)) => return Err(err.into()),
        Err(err) => {
            tracing::error!(error = %err, customer_id, "QuickBooks invoices failed");
            CustomerInvoices::empty(InvoicesStatus::Error)
        }
    })
}

/// The access token, once the invoice is known to be the customer's.
async fn owned_invoice(
    state: &AppState,
    company_id: i32,
    customer_id: i32,
    invoice_id: &str,
) -> ApiResult<Access> {
    if !valid_id(invoice_id) {
        return Err(ApiError::BadRequest("Invalid invoice".into()));
    }
    let expected = customer(state, company_id, customer_id)
        .await?
        .qbo_id
        .ok_or(ApiError::NotFound)?;
    let access = state
        .qbo
        .access(&state.pool, company_id)
        .await
        .map_err(qbo_error)?;
    let rows = state
        .qbo
        .query(
            &access,
            &format!(
                "select Id, CustomerRef from Invoice where Id = '{}'",
                escape(invoice_id)
            ),
            "Invoice",
        )
        .await
        .map_err(qbo_error)?;
    let owner = rows
        .first()
        .and_then(|row| text(&row["CustomerRef"]["value"]));
    if owner.as_deref() != Some(expected.as_str()) {
        return Err(ApiError::Forbidden);
    }
    Ok(access)
}

async fn fetch_invoice(
    state: &AppState,
    access: &Access,
    invoice_id: &str,
) -> Result<Value, QboError> {
    let body = state
        .qbo
        .get(access, &format!("invoice/{invoice_id}"))
        .await?;
    Ok(if body["Invoice"].is_object() {
        body["Invoice"].clone()
    } else {
        body
    })
}

/// Get a customer's QuickBooks invoice
///
/// One invoice of the customer, for the invoice editor.
#[crate::openapi::path(
    get,
    path = "/v1/customers/{id}/qbo-invoices/{invoice_id}",
    operation_id = "get_customer_qbo_invoice",
    tag = "quickbooks",
    params(
        ("id" = i32, Path, description = "Customer id"),
        ("invoice_id" = String, Path, description = "QuickBooks invoice id"),
    ),
    responses(
        (status = 200, description = "Invoice", body = EditableInvoice),
        (status = 400, description = "Invalid invoice id", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an invoice of this customer", body = ErrorBody),
        (status = 404, description = "No such customer, or it is not linked to QuickBooks", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_invoice(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((customer_id, invoice_id)): Path<(i32, String)>,
) -> ApiResult<Json<EditableInvoice>> {
    let access = owned_invoice(&state, user.company_id, customer_id, &invoice_id).await?;
    let raw = fetch_invoice(&state, &access, &invoice_id)
        .await
        .map_err(qbo_error)?;
    editable(&raw)
        .map(Json)
        .ok_or_else(|| ApiError::Upstream("Failed to parse invoice".into()))
}

/// Body of `PUT /v1/customers/{id}/qbo-invoices/{invoice_id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InvoiceUpdate {
    /// The invoice as the editor left it.
    pub invoice: EditableInvoice,
}

async fn save(
    state: &AppState,
    access: &Access,
    invoice_id: &str,
    edited: &EditableInvoice,
) -> Result<Value, ApiError> {
    let current = fetch_invoice(state, access, invoice_id)
        .await
        .map_err(qbo_error)?;
    let payload = update_payload(&current, edited).map_err(ApiError::BadRequest)?;
    state
        .qbo
        .post(access, "invoice", &payload)
        .await
        .map_err(qbo_error)
}

/// Save a customer's QuickBooks invoice
///
/// Writes the editor's lines, dates, notes and addresses to the invoice.
/// Lines the editor removed are deleted; discounts and tax stay. When
/// `QuickBooks` refuses a stale version the update is retried once on the
/// current one.
#[crate::openapi::path(
    put,
    path = "/v1/customers/{id}/qbo-invoices/{invoice_id}",
    operation_id = "update_customer_qbo_invoice",
    tag = "quickbooks",
    params(
        ("id" = i32, Path, description = "Customer id"),
        ("invoice_id" = String, Path, description = "QuickBooks invoice id"),
    ),
    request_body = InvoiceUpdate,
    responses(
        (status = 200, description = "The saved invoice", body = EditableInvoice),
        (status = 400, description = "Invalid invoice, or a sales line without an item", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an invoice of this customer", body = ErrorBody),
        (status = 404, description = "No such customer, or it is not linked to QuickBooks", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_invoice(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((customer_id, invoice_id)): Path<(i32, String)>,
    JsonBody(input): JsonBody<InvoiceUpdate>,
) -> ApiResult<Json<EditableInvoice>> {
    if input.invoice.id != invoice_id {
        return Err(ApiError::BadRequest("Invalid invoice payload".into()));
    }
    let access = owned_invoice(&state, user.company_id, customer_id, &invoice_id).await?;
    let saved = match save(&state, &access, &invoice_id, &input.invoice).await {
        Ok(saved) => saved,
        Err(ApiError::Upstream(_)) => save(&state, &access, &invoice_id, &input.invoice).await?,
        Err(err) => return Err(err),
    };
    let saved = if saved["Invoice"].is_object() {
        &saved["Invoice"]
    } else {
        &saved
    };
    editable(saved)
        .map(Json)
        .ok_or_else(|| ApiError::Upstream("Failed to parse updated invoice".into()))
}

fn pdf_response(invoice_id: &str, bytes: Vec<u8>) -> impl IntoResponse + use<> {
    (
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"invoice-{invoice_id}.pdf\""),
            ),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        bytes,
    )
}

/// Get a customer's QuickBooks invoice as a PDF
///
/// The invoice's PDF, once it is known to be this customer's.
#[crate::openapi::path(
    get,
    path = "/v1/customers/{id}/qbo-invoices/{invoice_id}/pdf",
    operation_id = "get_customer_qbo_invoice_pdf",
    tag = "quickbooks",
    params(
        ("id" = i32, Path, description = "Customer id"),
        ("invoice_id" = String, Path, description = "QuickBooks invoice id"),
    ),
    responses(
        (status = 200, description = "The PDF", content_type = "application/pdf", body = Vec<u8>),
        (status = 400, description = "Invalid invoice id", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an invoice of this customer", body = ErrorBody),
        (status = 404, description = "No such customer, or it is not linked to QuickBooks", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn customer_invoice_pdf(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((customer_id, invoice_id)): Path<(i32, String)>,
) -> ApiResult<impl IntoResponse> {
    customer_invoice_pdf_of(&state, user.company_id, customer_id, &invoice_id).await
}

/// The PDF of one of a customer's `QuickBooks` invoices (also for the
/// customer's own view).
pub async fn customer_invoice_pdf_of(
    state: &AppState,
    company_id: i32,
    customer_id: i32,
    invoice_id: &str,
) -> ApiResult<axum::response::Response> {
    let access = owned_invoice(state, company_id, customer_id, invoice_id).await?;
    let bytes = state
        .qbo
        .invoice_pdf(&access, invoice_id)
        .await
        .map_err(qbo_error)?;
    Ok(pdf_response(invoice_id, bytes).into_response())
}

/// Get a QuickBooks invoice as a PDF
///
/// Any invoice of the company's `QuickBooks`, for the rep's sales panel and
/// schedule events.
#[crate::openapi::path(
    get,
    path = "/v1/qbo/invoices/{invoice_id}/pdf",
    operation_id = "get_qbo_invoice_pdf",
    tag = "quickbooks",
    params(("invoice_id" = String, Path, description = "QuickBooks invoice id")),
    responses(
        (status = 200, description = "The PDF", content_type = "application/pdf", body = Vec<u8>),
        (status = 400, description = "Invalid invoice id", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn invoice_pdf(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(invoice_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    if !valid_id(&invoice_id) {
        return Err(ApiError::BadRequest("Invalid invoice".into()));
    }
    let access = state
        .qbo
        .access(&state.pool, user.company_id)
        .await
        .map_err(qbo_error)?;
    let bytes = state
        .qbo
        .invoice_pdf(&access, &invoice_id)
        .await
        .map_err(qbo_error)?;
    Ok(pdf_response(&invoice_id, bytes))
}

/// Query of `GET /v1/qbo/customers`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CustomersQuery {
    /// Exact primary email.
    pub email: Option<String>,
    /// Part of the display name (wins over `email`).
    pub name: Option<String>,
}

/// `GET /v1/qbo/customers`.
#[derive(Debug, Serialize, ToSchema)]
pub struct QboCustomers {
    /// False when the company has not connected `QuickBooks`.
    pub connected: bool,
    /// Matches.
    pub customers: Vec<QboCustomer>,
}

/// Find QuickBooks customers
///
/// `QuickBooks` customers whose display name contains `name` (up to 25), or
/// whose primary email is `email`.
#[crate::openapi::path(
    get,
    path = "/v1/qbo/customers",
    operation_id = "search_qbo_customers",
    tag = "quickbooks",
    params(CustomersQuery),
    responses(
        (status = 200, description = "Matches", body = QboCustomers),
        (status = 400, description = "Neither email nor name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn customers(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<CustomersQuery>,
) -> ApiResult<Json<QboCustomers>> {
    let name = query.name.as_deref().map_or("", str::trim);
    let email = query.email.as_deref().map_or("", str::trim);
    if name.is_empty() && email.is_empty() {
        return Err(ApiError::BadRequest("email or name is required".into()));
    }
    let access = match state.qbo.access(&state.pool, user.company_id).await {
        Ok(access) => access,
        Err(err) if err.is_not_connected() => {
            return Ok(Json(QboCustomers {
                connected: false,
                customers: Vec::new(),
            }));
        }
        Err(err) => return Err(qbo_error(err)),
    };
    let customers = if name.is_empty() {
        by_email(&state, &access, email)
            .await
            .map_err(qbo_error)?
            .iter()
            .filter_map(qbo_customer)
            .collect()
    } else {
        by_name(&state, &access, name).await.map_err(qbo_error)?
    };
    Ok(Json(QboCustomers {
        connected: true,
        customers,
    }))
}

#[cfg(test)]
mod tests {
    use super::{QboCustomer, pick_by_name};

    fn found(id: &str, name: &str, email: Option<&str>) -> QboCustomer {
        QboCustomer {
            id: id.into(),
            display_name: name.into(),
            email: email.map(Into::into),
        }
    }

    #[test]
    fn names_pick_an_exact_or_unique_match() {
        let list = [
            found("1", "Alice Lead", Some("a@x.test")),
            found("2", "Alice Lead", Some("b@x.test")),
            found("3", "Alice Leader", None),
        ];
        assert_eq!(
            pick_by_name(&list, "alice lead", "B@x.test"),
            Some("2".into())
        );
        assert_eq!(pick_by_name(&list, "alice lead", ""), None);
        assert_eq!(pick_by_name(&list[2..], "Alice", ""), Some("3".into()));
    }
}
