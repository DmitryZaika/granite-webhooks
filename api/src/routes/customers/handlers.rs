use super::queries::{emails_by_customer_ids, list_companies, list_customers};
use super::schemas::{
    CustomersView, EmailsBatchRequest, EmailsByCustomer, ListFilter, ListQuery, ListResponse,
    MAX_BATCH_IDS,
};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};

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
