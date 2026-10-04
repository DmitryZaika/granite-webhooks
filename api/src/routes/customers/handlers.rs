use super::queries::{emails_by_customer_ids, list_companies, list_customers};
use super::schemas::{
    CustomersView, EmailsBatchRequest, EmailsByCustomer, ListFilter, ListQuery, ListResponse,
    MAX_BATCH_IDS,
};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};

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
