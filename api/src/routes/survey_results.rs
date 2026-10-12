//! `GET /v1/survey-results`: what customers answered to the post-install
//! survey, for the admin Surveys page.
//!
//! Replaces the Remix `admin.surveys` loader.

use crate::auth::AdminUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::serde_helpers::js_date;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(results))
}

/// Query of `GET /v1/survey-results`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ResultsQuery {
    /// 30, 90 (default) or 365 days back; other values read as 90.
    pub days: Option<i64>,
}

/// One answered survey.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct SurveyAnswer {
    /// Survey id.
    pub id: i32,
    /// The sales rep's name.
    pub sales_rep_name: Option<String>,
    /// 1–5.
    pub sales_rep_rating: i32,
    /// What the customer said about the rep.
    pub sales_rep_comments: Option<String>,
    /// 1–5.
    pub technician_rating: i32,
    /// What the customer said about the technician.
    pub technician_comments: Option<String>,
    /// The installer's name.
    pub installer_name: Option<String>,
    /// 1–5.
    pub installation_rating: i32,
    /// What the customer said about the installation.
    pub installation_comments: Option<String>,
    /// When it was answered.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
}

/// A sales rep's average.
#[derive(Debug, Serialize, ToSchema)]
pub struct RepRating {
    /// Name ("Not specified" when the rep is gone).
    pub name: String,
    /// Average sales rep rating.
    pub rating: f64,
    /// Answers about them.
    pub answers: i64,
}

/// Survey totals of the range.
#[derive(Debug, Serialize, ToSchema)]
pub struct SurveyStats {
    /// Customers sent the survey.
    pub invited: i64,
    /// Surveys answered.
    pub answers: i64,
    /// Sales reps, best rated first.
    pub sales_reps: Vec<RepRating>,
}

/// The page's data.
#[derive(Debug, Serialize, ToSchema)]
pub struct SurveyResults {
    /// The range used.
    pub days: i64,
    /// Answers, newest first.
    pub items: Vec<SurveyAnswer>,
    /// Totals.
    pub stats: SurveyStats,
    /// Invitations sent per day, oldest first, one entry per day of the range.
    pub sent_by_day: Vec<i64>,
}

/// Invitation markers the survey scheduler leaves in `emails`.
const INVITATION: &str = "message_id REGEXP '^checklist-survey-[0-9]+$'";

/// Get survey results
///
/// The post-install survey answers of the last `days` days with the
/// invitation count, the answer count, each sales rep's average and the
/// invitations sent per day.
#[utoipa::path(
    get,
    path = "/v1/survey-results",
    operation_id = "get_survey_results",
    tag = "surveys",
    params(ResultsQuery),
    responses(
        (status = 200, description = "Results", body = SurveyResults),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn results(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    MultiQuery(query): MultiQuery<ResultsQuery>,
) -> ApiResult<Json<SurveyResults>> {
    let days = query
        .days
        .filter(|d| [30, 90, 365].contains(d))
        .unwrap_or(90);
    let pool = &state.pool;
    let company = user.company_id;
    let items: Vec<SurveyAnswer> = sqlx::query_as(
        "SELECT cs.id, cs.sales_rep_rating, cs.sales_rep_comment AS sales_rep_comments,
                cs.technician_rating, cs.technician_comment AS technician_comments,
                cs.installation_rating, cs.installation_comment AS installation_comments,
                cs.created_at, u.name AS sales_rep_name, i.name AS installer_name
           FROM customer_surveys cs
           LEFT JOIN users u ON cs.sales_rep_id = u.id
           LEFT JOIN users i ON cs.installer_id = i.id
          WHERE cs.company_id = ? AND cs.created_at >= DATE_SUB(NOW(), INTERVAL ? DAY)
          ORDER BY cs.created_at DESC, cs.id DESC",
    )
    .bind(company)
    .bind(days)
    .fetch_all(pool)
    .await?;
    let invited: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM emails
          WHERE company_id = ? AND {INVITATION}
            AND sent_at >= DATE_SUB(UTC_TIMESTAMP(), INTERVAL ? DAY)"
    )))
    .bind(company)
    .bind(days)
    .fetch_one(pool)
    .await?;
    let reps: Vec<(Option<String>, f64, i64)> = sqlx::query_as(
        "SELECT u.name, CAST(AVG(cs.sales_rep_rating) AS DOUBLE), COUNT(*)
           FROM customer_surveys cs
           LEFT JOIN users u ON cs.sales_rep_id = u.id
          WHERE cs.company_id = ? AND cs.created_at >= DATE_SUB(NOW(), INTERVAL ? DAY)
          GROUP BY cs.sales_rep_id, u.name
          ORDER BY 2 DESC, 3 DESC",
    )
    .bind(company)
    .bind(days)
    .fetch_all(pool)
    .await?;
    let sent: Vec<(i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(DATEDIFF(UTC_DATE(), DATE(sent_at)) AS SIGNED) AS ago, COUNT(*)
           FROM emails
          WHERE company_id = ? AND {INVITATION}
            AND sent_at >= DATE_SUB(UTC_TIMESTAMP(), INTERVAL ? DAY)
          GROUP BY ago"
    )))
    .bind(company)
    .bind(days)
    .fetch_all(pool)
    .await?;
    let mut sent_by_day = vec![0; usize::try_from(days).unwrap_or(90)];
    for (ago, count) in sent {
        if let Some(index) = (days - 1 - ago)
            .try_into()
            .ok()
            .filter(|index: &usize| *index < sent_by_day.len())
        {
            sent_by_day[index] += count;
        }
    }
    Ok(Json(SurveyResults {
        days,
        stats: SurveyStats {
            invited,
            answers: i64::try_from(items.len()).unwrap_or(i64::MAX),
            sales_reps: reps
                .into_iter()
                .map(|(name, rating, answers)| RepRating {
                    name: name
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| "Not specified".into()),
                    rating,
                    answers,
                })
                .collect(),
        },
        items,
        sent_by_day,
    }))
}
