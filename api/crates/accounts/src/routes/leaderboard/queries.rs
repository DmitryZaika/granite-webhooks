//! The points leaderboard query.
//!
//! Port of `loadPointsLeaderboardRows` (`pointsLeaderboard.server.ts`) and
//! `callSmsPointsCtes` (`callSmsPoints.server.ts`); the comments there explain
//! each rule. Keep the point values in sync with `app/utils/activityPoints.ts`
//! until the admin "Calls & SMS points" table moves here too.
//!
//! Every source counts only from its `activity_points_sources.counts_from`
//! (a source without a row scores nothing); "this month" also starts the
//! window at the 1st. Only Sales Reps of the company are ranked; the viewer,
//! when not one, gets their own unranked row.

use super::schemas::LeaderboardRow;
use chrono::NaiveDate;
use sqlx::{MySql, MySqlPool, QueryBuilder};

const SALES_REP_POSITION: i32 = 1;

/// Points per activity (`ACTIVITY_POINTS`).
mod points {
    pub const DEAL_WON: i64 = 25;
    pub const DEAL_STAGE_ADVANCE: i64 = 3;
    pub const CALL_ATTEMPT: i64 = 2;
    pub const CALL_VOICEMAIL: i64 = 5;
    pub const CALL_CONVERSATION: i64 = 10;
    pub const SMS_SENT: i64 = 1;
    pub const EMAIL_SENT: i64 = 1;
}

/// Per rep, per customer number, per day (`ACTIVITY_DAILY_CAP_PER_NUMBER`).
mod daily_cap {
    pub const CALL_ATTEMPT: i64 = 1;
    pub const CALL_VOICEMAIL: i64 = 1;
    pub const SMS_SENT: i64 = 3;
}

pub struct LeaderboardInput {
    pub company_id: i32,
    /// The signed-in user, scored unranked when not a rep.
    pub viewer_id: i32,
    /// Count guided tour points (the company's tours are on).
    pub include_tours: bool,
    /// Start of the window (inclusive); None for all time.
    pub from: Option<NaiveDate>,
}

/// A reporting window: `from` inclusive, `to` exclusive; open ends are unbounded.
#[derive(Debug, Clone, Copy, Default)]
pub struct Window {
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
}

/// `AND <column> >= <from> AND <column> < <to>` for the window's ends.
fn push_window(q: &mut QueryBuilder<MySql>, column: &str, window: Window) {
    if let Some(from) = window.from {
        q.push(format_args!(" AND {column} >= ")).push_bind(from);
    }
    if let Some(to) = window.to {
        q.push(format_args!(" AND {column} < ")).push_bind(to);
    }
}

// One query, written out in order: splitting it would scatter the binds.
#[allow(clippy::too_many_lines)]
pub async fn leaderboard_rows(
    pool: &MySqlPool,
    input: &LeaderboardInput,
) -> Result<Vec<LeaderboardRow>, sqlx::Error> {
    let LeaderboardInput {
        company_id,
        viewer_id,
        include_tours,
        from,
    } = *input;
    let mut q = QueryBuilder::<MySql>::new("WITH ");

    if include_tours {
        q.push(
            "task_totals AS (
               SELECT l.user_id, l.task_id, GREATEST(0, SUM(l.points)) AS task_points
                 FROM user_points_ledger l
                 JOIN activity_points_sources src ON src.source = 'tours'
                WHERE l.company_id = ",
        )
        .push_bind(company_id)
        .push(" AND l.awarded_at >= src.counts_from");
        push_window(&mut q, "l.awarded_at", Window { from, to: None });
        q.push(
            " GROUP BY l.user_id, l.task_id
             ),
             tour_totals AS (
               SELECT user_id, SUM(task_points) AS points FROM task_totals GROUP BY user_id
             ),",
        );
    }

    // A deal's updated_at is when it was marked won (see the Remix comments).
    q.push(" won_totals AS (SELECT d.user_id, COUNT(*) * ")
        .push_bind(points::DEAL_WON)
        .push(
            " AS points
                FROM deals d
                JOIN customers c ON c.id = d.customer_id
                JOIN activity_points_sources src ON src.source = 'deals_won'
               WHERE c.company_id = ",
        )
        .push_bind(company_id)
        .push(
            " AND d.is_won = 1 AND d.deleted_at IS NULL AND c.deleted_at IS NULL
              AND d.user_id IS NOT NULL AND d.updated_at >= src.counts_from",
        );
    push_window(&mut q, "d.updated_at", Window { from, to: None });
    q.push(" GROUP BY d.user_id),");

    // Each distinct (deal, stage) entered, except the deal's first row.
    q.push(
        " stage_totals AS (
            SELECT /*+ JOIN_ORDER(src, h, d, c) */
                   d.user_id, COUNT(DISTINCT h.deal_id, h.list_id) * ",
    )
    .push_bind(points::DEAL_STAGE_ADVANCE)
    .push(
        " AS points
            FROM deal_stage_history h
            JOIN deals d ON d.id = h.deal_id
            JOIN customers c ON c.id = d.customer_id
            JOIN activity_points_sources src ON src.source = 'deal_progress'
           WHERE h.entered_at >= src.counts_from AND c.company_id = ",
    )
    .push_bind(company_id);
    push_window(&mut q, "h.entered_at", Window { from, to: None });
    q.push(
        " AND d.deleted_at IS NULL AND c.deleted_at IS NULL AND d.user_id IS NOT NULL
          AND EXISTS (SELECT 1 FROM deal_stage_history h2
                       WHERE h2.deal_id = h.deal_id AND h2.id < h.id)
         GROUP BY d.user_id),",
    );

    push_call_sms_ctes(&mut q, company_id, Window { from, to: None });

    q.push(
        ", reps AS (
             SELECT DISTINCT up.user_id
               FROM users_positions up
               JOIN users ru ON ru.id = up.user_id
              WHERE up.company_id = ",
    )
    .push_bind(company_id)
    .push(" AND ru.company_id = ")
    .push_bind(company_id)
    .push(" AND up.position_id = ")
    .push_bind(SALES_REP_POSITION)
    .push(")");

    let tour_points = if include_tours {
        "COALESCE(t.points, 0)"
    } else {
        "0"
    };
    let total = format!(
        "{tour_points} + COALESCE(w.points, 0) + COALESCE(s.points, 0)
         + COALESCE(cs.call_points, 0) + COALESCE(cs.sms_points, 0)
         + COALESCE(cs.email_points, 0)"
    );
    q.push(format_args!(
        " SELECT u.id AS user_id,
                 COALESCE(NULLIF(TRIM(u.name), ''), NULLIF(TRIM(u.email), ''), 'Colleague')
                   AS name,
                 u.photo_url,
                 CAST({tour_points} AS SIGNED) AS tour_points,
                 CAST(COALESCE(w.points, 0) AS SIGNED) AS deal_points,
                 CAST(COALESCE(s.points, 0) AS SIGNED) AS stage_points,
                 CAST(COALESCE(cs.call_points, 0) AS SIGNED) AS call_points,
                 CAST(COALESCE(cs.sms_points, 0) AS SIGNED) AS sms_points,
                 CAST(COALESCE(cs.email_points, 0) AS SIGNED) AS email_points,
                 CAST(COALESCE(cs.conversations, 0) AS SIGNED) AS conversations,
                 CAST(COALESCE(cs.voicemails, 0) AS SIGNED) AS voicemails,
                 CAST(COALESCE(cs.attempts, 0) AS SIGNED) AS attempts,
                 CAST(COALESCE(cs.sms, 0) AS SIGNED) AS sms,
                 CAST(COALESCE(cs.emails, 0) AS SIGNED) AS emails,
                 CAST({total} AS SIGNED) AS points,
                 CAST(r.user_id IS NOT NULL AS SIGNED) AS is_rep,
                 -- Ranked among the reps only; the viewer's own row is NULL.
                 CAST(CASE WHEN r.user_id IS NOT NULL
                      THEN RANK() OVER (PARTITION BY r.user_id IS NOT NULL ORDER BY {total} DESC)
                 END AS SIGNED) AS rank_position
            FROM users u
            LEFT JOIN reps r ON r.user_id = u.id
            {tour_join}
            LEFT JOIN won_totals w ON w.user_id = u.id
            LEFT JOIN stage_totals s ON s.user_id = u.id
            LEFT JOIN call_sms_totals cs ON cs.user_id = u.id
           WHERE u.is_deleted = 0 AND (r.user_id IS NOT NULL OR u.id = ",
        tour_join = if include_tours {
            "LEFT JOIN tour_totals t ON t.user_id = u.id"
        } else {
            ""
        },
    ))
    .push_bind(viewer_id)
    .push(") ORDER BY points DESC, u.name ASC, u.id ASC");

    q.build_query_as::<LeaderboardRow>().fetch_all(pool).await
}

/// CTEs ending in `call_sms_totals(user_id, conversations, voicemails,
/// attempts, sms, emails, call_points, sms_points, email_points)`.
#[allow(clippy::too_many_lines)]
fn push_call_sms_ctes(q: &mut QueryBuilder<MySql>, company_id: i32, window: Window) {
    // Calls: a 'pending' call scores nothing; attempts and voicemails are
    // capped per rep, number and day; a NULL number never pools with another.
    q.push(
        " call_rows AS (
            SELECT pcp.user_id, pcp.outcome,
                   COALESCE(pcp.customer_digits, CONCAT('call:', pcp.id)) AS number_key,
                   DATE(pcp.started_at) AS day, pcp.started_at AS ts, pcp.id
              FROM phone_call_points pcp
              JOIN activity_points_sources src ON src.source = 'calls'
             WHERE pcp.company_id = ",
    )
    .push_bind(company_id)
    .push(
        " AND pcp.user_id IS NOT NULL AND pcp.outcome <> 'pending'
          AND pcp.started_at >= src.counts_from",
    );
    push_window(q, "pcp.started_at", window);
    q.push(
        "),
          call_ranked AS (
            SELECT user_id, outcome,
                   ROW_NUMBER() OVER (PARTITION BY user_id, number_key, day, outcome
                                      ORDER BY ts, id) AS rn
              FROM call_rows
          ),
          call_capped AS (
            SELECT user_id, outcome FROM call_ranked
             WHERE outcome = 'conversation'
                OR (outcome = 'attempt' AND rn <= ",
    )
    .push_bind(daily_cap::CALL_ATTEMPT)
    .push(") OR (outcome = 'voicemail' AND rn <= ")
    .push_bind(daily_cap::CALL_VOICEMAIL)
    .push(
        ")
          ),
          call_totals AS (
            SELECT user_id,
                   SUM(outcome = 'conversation') AS conversations,
                   SUM(outcome = 'voicemail') AS voicemails,
                   SUM(outcome = 'attempt') AS attempts,
                   SUM(CASE outcome WHEN 'conversation' THEN ",
    )
    .push_bind(points::CALL_CONVERSATION)
    .push(" WHEN 'voicemail' THEN ")
    .push_bind(points::CALL_VOICEMAIL)
    .push(" WHEN 'attempt' THEN ")
    .push_bind(points::CALL_ATTEMPT)
    .push(" ELSE 0 END) AS call_points FROM call_capped GROUP BY user_id),");

    // Texts: a text without a sender is credited to the user whose agent /
    // extension id sent it (one user per id).
    for (cte, column) in [
        ("sms_cloudtalk_agent_users", "cloudtalk_agent_id"),
        ("sms_ringcentral_agent_users", "ringcentral_extension_id"),
    ] {
        q.push(format_args!(
            " {cte} AS (
                SELECT TRIM({column}) AS agent, MIN(id) AS user_id
                  FROM users
                 WHERE company_id = "
        ))
        .push_bind(company_id)
        .push(format_args!(
            " AND is_deleted = 0 AND {column} IS NOT NULL AND TRIM({column}) <> ''
                 GROUP BY TRIM({column})),"
        ));
    }
    q.push(" sms_events AS (");
    for (index, (table, agents)) in [
        ("cloudtalk_sms", "sms_cloudtalk_agent_users"),
        ("ringcentral_sms", "sms_ringcentral_agent_users"),
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            q.push(" UNION ALL ");
        }
        q.push(format_args!(
            "SELECT COALESCE(m.sender_user_id, a.user_id) AS user_id,
                    COALESCE(m.recipient10, CONCAT('sms:', m.id)) AS number_key,
                    DATE(m.created_date) AS day, m.created_date AS ts, m.id
               FROM {table} m
               JOIN activity_points_sources src ON src.source = 'sms'
               LEFT JOIN {agents} a ON a.agent = TRIM(m.agent)
              WHERE m.company_id = "
        ))
        .push_bind(company_id)
        .push(
            " AND m.direction = 'outbound' AND m.status = 'sent' AND m.is_echo = 0
              AND m.is_automated = 0 AND m.created_date >= src.counts_from",
        );
        push_window(q, "m.created_date", window);
    }
    q.push(
        "),
          sms_ranked AS (
            SELECT user_id,
                   ROW_NUMBER() OVER (PARTITION BY user_id, number_key, day ORDER BY ts, id) AS rn
              FROM sms_events WHERE user_id IS NOT NULL
          ),
          sms_totals AS (
            SELECT user_id, COUNT(*) AS sms, COUNT(*) * ",
    )
    .push_bind(points::SMS_SENT)
    .push(" AS sms_points FROM sms_ranked WHERE rn <= ")
    .push_bind(daily_cap::SMS_SENT)
    .push(" GROUP BY user_id),");

    // Emails: sent (has a message id), not a draft, not sent by the app.
    q.push(" email_totals AS (SELECT e.sender_user_id AS user_id, COUNT(*) AS emails, COUNT(*) * ")
        .push_bind(points::EMAIL_SENT)
        .push(
            " AS email_points
                FROM emails e
                JOIN activity_points_sources src ON src.source = 'emails'
               WHERE e.company_id = ",
        )
        .push_bind(company_id)
        .push(
            " AND e.sender_user_id IS NOT NULL AND e.deleted_at IS NULL
              AND COALESCE(e.is_draft, 0) = 0 AND e.is_automated = 0
              AND e.message_id IS NOT NULL AND e.sent_at >= src.counts_from",
        );
    push_window(q, "e.sent_at", window);
    q.push(
        " GROUP BY e.sender_user_id),
          call_sms_users AS (
            SELECT user_id FROM call_totals
            UNION SELECT user_id FROM sms_totals
            UNION SELECT user_id FROM email_totals
          ),
          call_sms_totals AS (
            SELECT u.user_id,
                   COALESCE(c.conversations, 0) AS conversations,
                   COALESCE(c.voicemails, 0) AS voicemails,
                   COALESCE(c.attempts, 0) AS attempts,
                   COALESCE(s.sms, 0) AS sms,
                   COALESCE(m.emails, 0) AS emails,
                   COALESCE(c.call_points, 0) AS call_points,
                   COALESCE(s.sms_points, 0) AS sms_points,
                   COALESCE(m.email_points, 0) AS email_points
              FROM call_sms_users u
              LEFT JOIN call_totals c ON c.user_id = u.user_id
              LEFT JOIN sms_totals s ON s.user_id = u.user_id
              LEFT JOIN email_totals m ON m.user_id = u.user_id
          )",
    );
}

/// `positions.id` of check-in kiosks and installers: they make no calls.
const NO_CALLS_POSITIONS: [i32; 2] = [8, 6];

/// A user's calls and hand-sent texts in a window.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow, crate::openapi::ToSchema)]
pub struct CallSmsPointsRow {
    /// The user.
    pub user_id: i32,
    /// Their name, else their email, else "Colleague".
    pub name: String,
    /// Real conversations.
    pub conversations: i64,
    /// Voicemails left (capped per number and day).
    pub voicemails: i64,
    /// Unanswered attempts (capped per number and day).
    pub attempts: i64,
    /// Hand-sent texts (capped per number and day).
    pub sms: i64,
    /// Call and text points.
    pub points: i64,
}

/// The admin "Calls & SMS points" table.
///
/// Every live user of the company who can make calls (super users, and
/// everyone without a check-in or installer position) with their call and
/// text points in the window, best first.
pub async fn call_sms_points_by_user(
    pool: &MySqlPool,
    company_id: i32,
    window: Window,
) -> Result<Vec<CallSmsPointsRow>, sqlx::Error> {
    let mut q = QueryBuilder::<MySql>::new("WITH");
    push_call_sms_ctes(&mut q, company_id, window);
    q.push(
        " SELECT u.id AS user_id,
                 COALESCE(NULLIF(TRIM(u.name), ''), NULLIF(TRIM(u.email), ''), 'Colleague') AS name,
                 CAST(COALESCE(t.conversations, 0) AS SIGNED) AS conversations,
                 CAST(COALESCE(t.voicemails, 0) AS SIGNED) AS voicemails,
                 CAST(COALESCE(t.attempts, 0) AS SIGNED) AS attempts,
                 CAST(COALESCE(t.sms, 0) AS SIGNED) AS sms,
                 CAST(COALESCE(t.call_points, 0) + COALESCE(t.sms_points, 0) AS SIGNED) AS points
            FROM users u
            LEFT JOIN call_sms_totals t ON t.user_id = u.id
           WHERE u.company_id = ",
    )
    .push_bind(company_id)
    .push(
        " AND u.is_deleted = 0
          AND (u.is_superuser = 1 OR NOT EXISTS (
                SELECT 1 FROM users_positions up
                 WHERE up.user_id = u.id AND up.company_id = u.company_id
                   AND up.position_id IN (",
    )
    .push_bind(NO_CALLS_POSITIONS[0])
    .push(", ")
    .push_bind(NO_CALLS_POSITIONS[1])
    .push("))) ORDER BY points DESC, u.name ASC, u.id ASC");
    q.build_query_as().fetch_all(pool).await
}
