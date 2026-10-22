use super::queries::{LeaderboardInput, leaderboard_rows};
use super::schemas::{LeaderboardPeriod, LeaderboardQuery, PointsLeaderboard};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::period::{MonthToDate, today};
use crate::routes::company::queries::company_features;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;

/// Get the points leaderboard
///
/// Ranks the Sales Reps of the company the request acts in by the points they
/// earned: deals won, deals moved forward, calls, hand-sent texts and emails,
/// and guided tours (only while the company's tours are on). `period=all`
/// (default) counts everything since each source started counting,
/// `period=month` this month so far. When the signed-in user is not a Sales
/// Rep, `viewer` holds their own unranked row. Answers 404 while the
/// company's leaderboard is switched off (see `GET /v1/company/features`).
#[utoipa::path(
    get,
    path = "/v1/points-leaderboard",
    operation_id = "get_points_leaderboard",
    tag = "leaderboard",
    params(LeaderboardQuery),
    responses(
        (status = 200, description = "Reps ranked by points", body = PointsLeaderboard),
        (status = 400, description = "Unknown period", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "The company's leaderboard is switched off", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn points_leaderboard(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<LeaderboardQuery>,
) -> ApiResult<Json<PointsLeaderboard>> {
    let features = company_features(&state.pool, user.company_id).await?;
    if !features.leaderboard {
        return Err(ApiError::NotFound);
    }
    let from = match query.period {
        LeaderboardPeriod::All => None,
        LeaderboardPeriod::Month => Some(MonthToDate::containing(today()).start),
    };
    let rows = leaderboard_rows(
        &state.pool,
        &LeaderboardInput {
            company_id: user.company_id,
            viewer_id: user.id,
            include_tours: features.onboarding,
            from,
        },
    )
    .await?;
    let mut entries = Vec::new();
    let mut viewer = None;
    for row in rows {
        if row.is_rep != 0 {
            entries.push(row.into());
        } else if row.user_id == user.id {
            viewer = Some(row.into());
        }
    }
    Ok(Json(PointsLeaderboard {
        period: query.period,
        includes_tours: features.onboarding,
        entries,
        viewer,
    }))
}
