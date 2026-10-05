mod common;

use api::auth::cookie::SessionData;
use common::{WithSession, cookie_for, ids, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

fn sorted(mut v: Vec<i64>) -> Vec<i64> {
    v.sort_unstable();
    v
}

// ---- GET /v1/users/sales-reps ----------------------------------------------

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn sales_reps_have_the_remix_shape(pool: MySqlPool) {
    let rows: Value = server(pool)
        .get("/v1/users/sales-reps")
        .with_session(sessions::REP)
        .await
        .json();
    let mut rows = rows.as_array().expect("array").clone();
    rows.sort_by_key(|row| row["id"].as_i64());
    assert_eq!(
        Value::Array(rows),
        json!([
            { "id": 100, "name": "Rita Rep" },
            { "id": 101, "name": "Mark Manager" }
        ])
    );
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn sales_reps_skip_deleted_users_and_other_companies(pool: MySqlPool) {
    let server = server(pool);

    // 105 is deleted; 106 belongs to company 101.
    let rows: Value = server
        .get("/v1/users/sales-reps")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(sorted(ids(&rows)), vec![100, 101]);

    // 101 holds the position in company 101 but is a user of company 100.
    let rows: Value = server
        .get("/v1/users/sales-reps")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(ids(&rows), vec![106]);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn sales_reps_follow_the_super_admin_company_switch(pool: MySqlPool) {
    let server = server(pool);
    let super_in = |company_id| {
        cookie_for(SessionData {
            session_id: Some(sessions::SUPER_ADMIN.into()),
            active_company_id: Some(company_id),
        })
    };

    let rows: Value = server
        .get("/v1/users/sales-reps")
        .add_header("cookie", super_in(100))
        .await
        .json();
    assert_eq!(sorted(ids(&rows)), vec![100, 101]);

    let rows: Value = server
        .get("/v1/users/sales-reps")
        .add_header("cookie", super_in(101))
        .await
        .json();
    assert_eq!(ids(&rows), vec![106]);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn sales_reps_require_an_employee(pool: MySqlPool) {
    let server = server(pool);
    server
        .get("/v1/users/sales-reps")
        .await
        .assert_status_unauthorized();
    server
        .get("/v1/users/sales-reps")
        .with_session(sessions::VIEWER)
        .await
        .assert_status_forbidden();
}
