mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed ("Employee dashboard" section): every points source counts from
// 2026-01-01; company 100 has the leaderboard on and guided tours off. Rita
// (100) is its only live Sales Rep:
// - won deals 8000, 8010 (this month) and 8004 (last month): 25 each
// - stage moves: 8002 this month, 8000 last month: 3 each
// - calls: a conversation (10) and one of two attempts to one number (2) this
//   month, a voicemail (5) last month; a pending call scores nothing
// - texts: three of four to one number on one day (1 each); an automated one never
// - emails: two sent this month, one last month (1 each); a draft and one
//   never sent do not count
// - tours: 40 points, counted only while tours are on
// Mark (101, manager) won 8008 this month (25).

const LEADERBOARD: &str = "/v1/points-leaderboard";

async fn board(pool: MySqlPool, query: &str, session: &str) -> Value {
    server(pool)
        .get(&format!("{LEADERBOARD}{query}"))
        .with_session(session)
        .await
        .json()
}

async fn run(pool: &MySqlPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

fn names_and_points(board: &Value) -> Vec<(String, i64, Option<i64>)> {
    board["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["name"].as_str().unwrap().to_string(),
                entry["points"].as_i64().unwrap(),
                entry["rank"].as_i64(),
            )
        })
        .collect()
}

#[test_db::test(seed)]
async fn all_time_board_scores_every_source(pool: MySqlPool) {
    assert_eq!(
        board(pool, "", sessions::REP).await,
        json!({
            "period": "all",
            "includes_tours": false,
            "entries": [{
                "user_id": 100,
                "name": "Rita Rep",
                "points": 104,
                "rank": 1,
                "breakdown": { "tours": 0, "deals": 75, "progress": 6, "calls": 17, "sms": 3, "emails": 3 },
                "activity": { "conversations": 1, "voicemails": 1, "attempts": 1, "sms": 3, "emails": 3 }
            }],
            "viewer": null
        })
    );
}

#[test_db::test(seed)]
async fn month_board_starts_at_the_first(pool: MySqlPool) {
    let month = board(pool, "?period=month", sessions::REP).await;
    assert_eq!(month["period"], "month");
    assert_eq!(
        month["entries"][0],
        json!({
            "user_id": 100,
            "name": "Rita Rep",
            "points": 70,
            "rank": 1,
            "breakdown": { "tours": 0, "deals": 50, "progress": 3, "calls": 12, "sms": 3, "emails": 2 },
            "activity": { "conversations": 1, "voicemails": 0, "attempts": 1, "sms": 3, "emails": 2 }
        })
    );
}

#[test_db::test(seed)]
async fn a_viewer_who_is_not_a_rep_gets_an_unranked_row(pool: MySqlPool) {
    let manager = board(pool, "", sessions::MANAGER).await;
    assert_eq!(
        names_and_points(&manager),
        vec![("Rita Rep".into(), 104, Some(1))]
    );
    assert_eq!(
        manager["viewer"],
        json!({
            "user_id": 101,
            "name": "Mark Manager",
            "points": 25,
            "rank": null,
            "breakdown": { "tours": 0, "deals": 25, "progress": 0, "calls": 0, "sms": 0, "emails": 0 },
            "activity": { "conversations": 0, "voicemails": 0, "attempts": 0, "sms": 0, "emails": 0 }
        })
    );
}

#[test_db::test(seed)]
async fn tour_points_count_while_tours_are_on(pool: MySqlPool) {
    // A task reset after it was finished nets out, never below 0.
    run(
        &pool,
        "UPDATE company SET onboarding_enabled = 1 WHERE id = 100;
         INSERT INTO user_points_ledger (id, user_id, company_id, task_id, points, reason, awarded_at) VALUES
           (9900, 100, 100, 'dashboard-goal', 20, 'completed', '2026-02-01'),
           (9901, 100, 100, 'dashboard-goal', -30, 'reset', '2026-02-02'),
           (9902, 100, 100, 'dashboard-arrange', 15, 'completed', '2025-06-01');",
    )
    .await;
    let all = board(pool, "", sessions::REP).await;
    assert_eq!(all["includes_tours"], true);
    // 40 (seed) + 0 (clamped) + nothing before the tours' start date.
    assert_eq!(all["entries"][0]["breakdown"]["tours"], 40);
    assert_eq!(all["entries"][0]["points"], 144);
}

#[test_db::test(seed)]
async fn reps_are_ranked_with_ties_and_name_fallbacks(pool: MySqlPool) {
    // Nina ties Rita (both rank 1, name order); a rep with a blank name is
    // shown by email, with 0 points and the next rank after the tie. A
    // deleted rep is never listed.
    run(
        &pool,
        "INSERT INTO users (id, email, password, name, is_employee, is_deleted, company_id) VALUES
           (900, 'nina@acme.test', 'x', 'Nina', 1, 0, 100),
           (901, 'blank@acme.test', 'x', '  ', 1, 0, 100),
           (902, 'gone@acme.test', 'x', 'Gone', 1, 1, 100);
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES
           (900, 1, 100), (901, 1, 100), (902, 1, 100);
         INSERT INTO deals (id, customer_id, amount, list_id, position, user_id, is_won, updated_at) VALUES
           (9900, 1000, 1, 101, 9, 900, 1, '2026-02-01'),
           (9901, 1000, 1, 101, 9, 900, 1, '2026-02-01'),
           (9902, 1000, 1, 101, 9, 900, 1, '2026-02-01'),
           (9903, 1000, 1, 101, 9, 900, 1, '2026-02-01');
         INSERT INTO emails (id, subject, body, sent_at, message_id, sender_user_id, company_id) VALUES
           (9900, 's', 'b', '2026-02-01', '<n1>', 900, 100),
           (9901, 's', 'b', '2026-02-01', '<n2>', 900, 100),
           (9902, 's', 'b', '2026-02-01', '<n3>', 900, 100),
           (9903, 's', 'b', '2026-02-01', '<n4>', 900, 100);",
    )
    .await;
    let all = board(pool, "", sessions::REP).await;
    assert_eq!(
        names_and_points(&all),
        vec![
            ("Nina".into(), 104, Some(1)),
            ("Rita Rep".into(), 104, Some(1)),
            ("blank@acme.test".into(), 0, Some(3)),
        ]
    );
}

#[test_db::test(seed)]
async fn sources_without_a_start_date_score_nothing(pool: MySqlPool) {
    run(
        &pool,
        "DELETE FROM activity_points_sources WHERE source IN ('deals_won', 'calls');",
    )
    .await;
    let all = board(pool, "", sessions::REP).await;
    assert_eq!(all["entries"][0]["breakdown"]["deals"], 0);
    assert_eq!(all["entries"][0]["breakdown"]["calls"], 0);
    assert_eq!(all["entries"][0]["points"], 12);
}

#[test_db::test(seed)]
async fn the_board_follows_the_company_switch_and_validates_the_period(pool: MySqlPool) {
    let server = server(pool);
    // Company 101 has the leaderboard off.
    server
        .get(LEADERBOARD)
        .with_session(sessions::OTHER_COMPANY_REP)
        .expect_failure()
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get(&format!("{LEADERBOARD}?period=week"))
        .with_session(sessions::REP)
        .expect_failure()
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .get(LEADERBOARD)
        .expect_failure()
        .await
        .assert_status_unauthorized();
    server
        .get(LEADERBOARD)
        .with_session(sessions::VIEWER)
        .expect_failure()
        .await
        .assert_status_forbidden();
}
