use super::queries::{
    customers_page, emails_by_customer_ids, find_customer, list_companies, list_customers,
    search_customers,
};
use super::schemas::{
    CustomerDetail, CustomerQuery, CustomerSearchQuery, CustomerSearchRow, CustomersPage,
    CustomersView, EmailsBatchRequest, EmailsByCustomer, ListFilter, ListQuery, ListResponse,
    MAX_BATCH_IDS, PageQuery, PageRequest,
};
use chrono::Utc;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};

/// List the company's customers
///
/// Returns every non-deleted customer of the caller's company, optionally
/// filtered by sales rep. Invalid leads are hidden unless `show_invalid=1`.
/// With `view=companies` it returns only customers that have a company name,
/// each with revenue and project totals. Rows carry the primary email only;
/// use `POST /v1/customers/emails/batch` for all addresses.
#[utoipa::path(
    get,
    path = "/v1/customers",
    operation_id = "list_customers",
    tag = "customers",
    params(ListQuery),
    responses(
        (status = 200, description = "Customer rows, or company rows when `view=companies`", body = ListResponse),
        (status = 400, description = "`sales_rep` is not an integer", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ListResponse>> {
    let filter = ListFilter::try_from(query)?;
    let response = match filter.view {
        CustomersView::Customers => {
            ListResponse::Customers(list_customers(&state.pool, user.company_id, filter).await?)
        }
        CustomersView::Companies => {
            ListResponse::Companies(list_companies(&state.pool, user.company_id, filter).await?)
        }
    };
    Ok(Json(response))
}

/// One page of the company's customers
///
/// The customers page's table, filtered, sorted and paged here instead of
/// in the browser. Filters as `GET /v1/customers`, plus the tab's lead
/// sources; the onboarding sample customer is left out. Each page carries
/// every email of its customers, and `month_rows` the rows behind the
/// "this month" counts per sales rep.
#[utoipa::path(
    get,
    path = "/v1/customers/page",
    operation_id = "list_customers_page",
    tag = "customers",
    params(PageQuery),
    responses(
        (status = 200, description = "The page", body = CustomersPage),
        (status = 400, description = "`sales_rep` is not an integer", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn page(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<CustomersPage>> {
    let request = PageRequest::try_from(query)?;
    let page = customers_page(&state.pool, user.company_id, request, Utc::now()).await?;
    Ok(Json(page))
}

/// Get all emails of several customers
///
/// Returns every email address of the given customers, primary address
/// first, keyed by customer id. Customers without emails, and ids belonging
/// to another company, are absent from the result.
#[utoipa::path(
    post,
    path = "/v1/customers/emails/batch",
    operation_id = "get_customer_emails",
    tag = "customers",
    request_body = EmailsBatchRequest,
    responses(
        (status = 200, description = "Map of customer id to email addresses, primary first", body = inline(std::collections::BTreeMap<String, Vec<String>>), example = json!({"1000": ["primary@example.com", "other@example.com"]})),
        (status = 400, description = "More than 20000 ids, or a malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn emails_batch(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Json(body): Json<EmailsBatchRequest>,
) -> ApiResult<Json<EmailsByCustomer>> {
    if body.customer_ids.len() > MAX_BATCH_IDS {
        return Err(ApiError::BadRequest(format!(
            "at most {MAX_BATCH_IDS} customer_ids per request"
        )));
    }
    let emails = emails_by_customer_ids(&state.pool, user.company_id, &body.customer_ids).await?;
    Ok(Json(emails))
}

/// Get one customer
///
/// Returns a customer of the caller's company with contact details, every
/// email address (primary first), the assigned sales rep and the lead form
/// answers. 404 for other companies' customers, and for deleted ones unless
/// `include_deleted=true` (a deal page still shows its deleted customer).
#[utoipa::path(
    get,
    path = "/v1/customers/{id}",
    operation_id = "get_customer",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id"), CustomerQuery),
    responses(
        (status = 200, description = "The customer", body = CustomerDetail),
        (status = 400, description = "Malformed query", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    MultiQuery(query): MultiQuery<CustomerQuery>,
) -> ApiResult<Json<CustomerDetail>> {
    find_customer(&state.pool, user.company_id, id, query.include_deleted)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// Search the company's customers
///
/// Returns up to 15 live customers matching `term` by name (typos
/// tolerated, also matches the company name), phone digits, any email, or
/// company name. Customers with a live deal of the caller come first, then
/// the caller's own customers, then the best text matches. Each row carries
/// every email and the customer's latest deal. Fetch one in full with
/// `GET /v1/customers/{id}`.
#[utoipa::path(
    get,
    path = "/v1/customers/search",
    operation_id = "search_customers",
    tag = "customers",
    params(CustomerSearchQuery),
    responses(
        (status = 200, description = "Matching customers, best first", body = Vec<CustomerSearchRow>),
        (status = 400, description = "Missing `term` or unknown `by`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn search(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<CustomerSearchQuery>,
) -> ApiResult<Json<Vec<CustomerSearchRow>>> {
    Ok(Json(
        search_customers(&state.pool, user.company_id, user.id, &query).await?,
    ))
}
