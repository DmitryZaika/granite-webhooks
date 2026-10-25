use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn admins_read_survey_results(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO customer_surveys (id, sales_rep_id, sales_rep_rating, sales_rep_comment, technician_rating,
                technician_comment, installation_rating, installation_comment, company_id, installer_id, created_at) VALUES
           (9500, 100, 5, 'Great', 4, NULL, 5, 'Clean', 100, 101, NOW() - INTERVAL 1 DAY),
           (9501, 100, 3, NULL, 3, NULL, 3, NULL, 100, NULL, NOW() - INTERVAL 2 DAY),
           (9502, 101, 4, NULL, 4, NULL, 4, NULL, 100, NULL, NOW() - INTERVAL 40 DAY),
           (9503, 106, 1, NULL, 1, NULL, 1, NULL, 101, NULL, NOW());
         INSERT INTO emails (id, subject, body, sent_at, message_id, company_id, is_draft) VALUES
           (9510, 'Survey', 'b', UTC_TIMESTAMP(), 'checklist-survey-9000', 100, 0),
           (9511, 'Survey', 'b', UTC_TIMESTAMP() - INTERVAL 1 DAY, 'checklist-survey-9001', 100, 0),
           (9512, 'Other', 'b', UTC_TIMESTAMP(), 'checklist-survey-x', 100, 0);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let body: Value = server
        .get("/v1/survey-results?days=30")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(body["days"], 30);
    let ids: Vec<i64> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [9500, 9501]);
    assert_eq!(body["items"][0]["installer_name"], "Mark Manager");
    assert_eq!(body["items"][0]["sales_rep_comments"], "Great");
    assert_eq!(
        body["stats"],
        json!({ "invited": 2, "answers": 2, "sales_reps": [{ "name": "Rita Rep", "rating": 4.0, "answers": 2 }] })
    );
    let days = body["sent_by_day"].as_array().unwrap();
    assert_eq!(days.len(), 30);
    assert_eq!(days[29], 1);
    assert_eq!(days[28], 1);

    let wider: Value = server
        .get("/v1/survey-results?days=7")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(wider["days"], 90);
    assert_eq!(wider["stats"]["answers"], 3);
    server
        .get("/v1/survey-results")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}
