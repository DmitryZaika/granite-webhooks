use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn seed(pool: &MySqlPool) {
    sqlx::raw_sql(
        r#"UPDATE users SET is_superuser = 1 WHERE id = 102;
           INSERT INTO message_feedback (id, history, feedback_text, created_at, resolved_at, user_id, company_id,
                                         status, manual_section_id, proposed_manual_change) VALUES
             (9801, '{"question":"How do I add a deal?","originalAnswer":"Click Add"}', 'wrong', '2026-10-01 10:00:00',
              NULL, 100, 100, 'user_approved', 'deals.add', 'Open Deals, then press Add deal.'),
             (9802, '{"question":"Q2","originalAnswer":"A2"}', NULL, '2026-10-02 10:00:00',
              NULL, 106, 101, 'escalated', NULL, NULL),
             (9803, '{"question":"Q3","originalAnswer":"A3"}', NULL, '2026-10-03 10:00:00',
              '2026-10-04 10:00:00', 100, 100, 'escalated', NULL, NULL),
             (9804, '{"question":"Q4","originalAnswer":"A4"}', NULL, '2026-10-05 10:00:00',
              NULL, 100, 100, 'liked', NULL, NULL);"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn superusers_see_open_threads_newest_first(pool: MySqlPool) {
    seed(&pool).await;
    let server = server(pool);
    server
        .get("/v1/superuser/manual-feedback")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let rows: Value = server
        .get("/v1/superuser/manual-feedback")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let ids: Vec<i64> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [9802, 9801]);
    assert_eq!(
        rows[1],
        json!({
            "id": 9801, "user_id": 100, "company_id": 100, "company_name": "Acme Stone",
            "user_name": "Rita Rep",
            "history": { "question": "How do I add a deal?", "originalAnswer": "Click Add" },
            "feedback_text": "wrong", "status": "user_approved", "manual_section_id": "deals.add",
            "proposed_manual_change": "Open Deals, then press Add deal.",
            "created_at": "2026-10-01T10:00:00Z"
        })
    );
}

#[test_db::test(seed)]
async fn approving_writes_the_manual_and_dismissing_does_not(pool: MySqlPool) {
    seed(&pool).await;
    let server = server(pool.clone());
    server
        .post("/v1/superuser/manual-feedback/9801/resolve")
        .with_session(sessions::ADMIN)
        .json(&json!({ "apply_manual_change": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let saved: (String, i32) = sqlx::query_as(
        "SELECT content, feedback_id FROM crm_manual_overrides WHERE section_id = 'deals.add'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(saved, ("Open Deals, then press Add deal.".into(), 9801));
    server
        .post("/v1/superuser/manual-feedback/9802/resolve")
        .with_session(sessions::ADMIN)
        .json(&json!({ "apply_manual_change": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_feedback WHERE id IN (9801, 9802) AND resolved_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(open, 0);
    server
        .post("/v1/superuser/manual-feedback/99999/resolve")
        .with_session(sessions::ADMIN)
        .json(&json!({ "apply_manual_change": false }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
