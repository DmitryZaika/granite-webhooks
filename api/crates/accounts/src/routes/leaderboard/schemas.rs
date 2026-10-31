use crate::openapi::{IntoParams, ToSchema};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// Which points count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum LeaderboardPeriod {
    /// Everything since each source started counting.
    #[default]
    All,
    /// This month so far (from the 1st, UTC).
    Month,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LeaderboardQuery {
    /// `all` (default) or `month`.
    #[serde(default)]
    #[param(inline)]
    pub period: LeaderboardPeriod,
}

/// The company's points leaderboard for one period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PointsLeaderboard {
    /// The period counted.
    pub period: LeaderboardPeriod,
    /// Whether guided tour points count (only while the company's guided
    /// tours are on; otherwise `breakdown.tours` is always 0).
    pub includes_tours: bool,
    /// The company's Sales Reps, most points first (then by name), ranked
    /// among themselves; reps with no points are listed with 0.
    pub entries: Vec<LeaderboardEntry>,
    /// The signed-in user's own row, scored the same way but not ranked, when
    /// they are not one of the Sales Reps (e.g. a manager); null otherwise.
    /// Only they see it.
    pub viewer: Option<LeaderboardEntry>,
}

/// One person's points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct LeaderboardEntry {
    /// User id.
    pub user_id: i32,
    /// Display name: the user's name, else their email, else "Colleague".
    pub name: String,
    /// Their picture (`PUT /v1/me/photo`); null without one.
    pub photo_url: Option<String>,
    /// Total points.
    pub points: i64,
    /// Rank among the reps (ties share a rank, 1 is the top); null for the
    /// `viewer` row.
    pub rank: Option<i64>,
    /// Points per source.
    pub breakdown: PointsBreakdown,
    /// What the call, text and email points were earned with.
    pub activity: LeaderboardActivity,
}

/// Points per source. Each source counts only from its own start date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct PointsBreakdown {
    /// Guided tours finished (each task's points net of resets, never below 0).
    pub tours: i64,
    /// Deals marked won (25 each).
    pub deals: i64,
    /// Deals moved to another stage (3 per stage entered; creating a deal scores nothing).
    pub progress: i64,
    /// Calls: 10 per conversation, 5 per voicemail left, 2 per attempt.
    pub calls: i64,
    /// Texts sent by hand (1 each).
    pub sms: i64,
    /// Emails sent by hand (1 each).
    pub emails: i64,
}

/// Counts behind the call, text and email points, after the daily caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct LeaderboardActivity {
    /// Calls that were a real conversation (over 30 s).
    pub conversations: i64,
    /// Calls that left a voicemail (at most one per number per day scores).
    pub voicemails: i64,
    /// Short calls (at most one per number per day scores).
    pub attempts: i64,
    /// Texts sent by hand (at most three per number per day score).
    pub sms: i64,
    /// Emails sent by hand.
    pub emails: i64,
}

#[derive(Debug, FromRow)]
pub struct LeaderboardRow {
    pub user_id: i32,
    pub name: String,
    pub photo_url: Option<String>,
    pub points: i64,
    pub tour_points: i64,
    pub deal_points: i64,
    pub stage_points: i64,
    pub call_points: i64,
    pub sms_points: i64,
    pub email_points: i64,
    pub conversations: i64,
    pub voicemails: i64,
    pub attempts: i64,
    pub sms: i64,
    pub emails: i64,
    pub is_rep: i64,
    pub rank_position: Option<i64>,
}

impl From<LeaderboardRow> for LeaderboardEntry {
    fn from(row: LeaderboardRow) -> Self {
        Self {
            user_id: row.user_id,
            name: row.name,
            photo_url: row.photo_url,
            points: row.points,
            rank: row.rank_position,
            breakdown: PointsBreakdown {
                tours: row.tour_points,
                deals: row.deal_points,
                progress: row.stage_points,
                calls: row.call_points,
                sms: row.sms_points,
                emails: row.email_points,
            },
            activity: LeaderboardActivity {
                conversations: row.conversations,
                voicemails: row.voicemails,
                attempts: row.attempts,
                sms: row.sms,
                emails: row.emails,
            },
        }
    }
}
