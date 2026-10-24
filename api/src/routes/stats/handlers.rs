use super::MESSAGE_MONTHS;
use super::queries::{
    deal_conversion as conversion_counts, sales_stats, sent_emails_by_month, sent_sms_by_month,
    top_sold_stones,
};
use super::schemas::{DealConversion, MonthlyCount, SalesStats, SentSms, TopSoldStone};
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::period::{MonthToDate, month_start_back, today};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;

/// Get my sales this month
///
/// Returns the signed-in user's deals this month so far (from the 1st through
/// today, UTC): how many were won and lost and the amount won, how many were
/// created, and the open pipeline (count and amount, whenever created). Only
/// deals assigned to the user, of live customers of the company and in live
/// deal lists, count.
#[utoipa::path(
    get,
    path = "/v1/me/stats/sales",
    operation_id = "get_my_sales_stats",
    tag = "stats",
    responses(
        (status = 200, description = "This month's deal numbers", body = SalesStats),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn sales(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<SalesStats>> {
    let month = MonthToDate::containing(today());
    Ok(Json(
        sales_stats(&state.pool, user.id, user.company_id, month).await?,
    ))
}

/// Get my deal conversion
///
/// Returns how many of the signed-in user's deals were won out of all of
/// them, overall and by how the customer came in (leads, walk-in, call-in).
/// All time; deals of deleted customers or in deleted lists are left out.
#[utoipa::path(
    get,
    path = "/v1/me/stats/deal-conversion",
    operation_id = "get_my_deal_conversion",
    tag = "stats",
    responses(
        (status = 200, description = "Won and total deals by customer source", body = DealConversion),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn deal_conversion(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<DealConversion>> {
    let row = conversion_counts(&state.pool, user.id, user.company_id).await?;
    Ok(Json(row.into()))
}

/// List the stones I sold most this month
///
/// Returns up to 3 stones the signed-in user sold the most full slabs of this
/// month so far (sales not cancelled; remnants not counted), most slabs first,
/// then most square feet. Empty when they sold none.
#[utoipa::path(
    get,
    path = "/v1/me/stats/top-stones",
    operation_id = "list_my_top_sold_stones",
    tag = "stats",
    responses(
        (status = 200, description = "Top stones, best first", body = Vec<TopSoldStone>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn top_stones(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<TopSoldStone>>> {
    let month = MonthToDate::containing(today());
    Ok(Json(
        top_sold_stones(&state.pool, user.id, user.company_id, month).await?,
    ))
}

/// List my sent emails per month
///
/// Returns how many emails the signed-in user sent in each of the current
/// month and the two before it (UTC), oldest first. Months without emails are
/// left out. Counts emails of the company and ones with no company.
#[utoipa::path(
    get,
    path = "/v1/me/stats/sent-emails",
    operation_id = "list_my_sent_emails_per_month",
    tag = "stats",
    responses(
        (status = 200, description = "Emails sent per month", body = Vec<MonthlyCount>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn sent_emails(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<MonthlyCount>>> {
    let today = today();
    let from = month_start_back(today, MESSAGE_MONTHS - 1);
    Ok(Json(
        sent_emails_by_month(&state.pool, user.id, user.company_id, from, today).await?,
    ))
}

/// List my sent texts per month
///
/// Returns the company's SMS provider (RingCentral when both RingCentral and
/// CloudTalk are set up) and how many texts the signed-in user sent through
/// it in each of the current month and the two before it (UTC), oldest first.
/// Counts sent outbound texts sent by the user or through their CloudTalk
/// agent / RingCentral extension. Months without texts are left out; with no
/// provider, `provider` is null and `months` empty.
#[utoipa::path(
    get,
    path = "/v1/me/stats/sent-sms",
    operation_id = "list_my_sent_sms_per_month",
    tag = "stats",
    responses(
        (status = 200, description = "The provider and texts sent per month", body = SentSms),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn sent_sms(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<SentSms>> {
    // The user's provider: Telnyx when they text from a Telnyx line.
    let provider =
        crate::routes::sms::send::user_provider(&state.pool, user.company_id, user.id).await?;
    let months = match provider {
        Some(provider) => {
            let today = today();
            let from = month_start_back(today, MESSAGE_MONTHS - 1);
            sent_sms_by_month(&state.pool, provider, user.id, user.company_id, from, today).await?
        }
        None => Vec::new(),
    };
    Ok(Json(SentSms { provider, months }))
}
