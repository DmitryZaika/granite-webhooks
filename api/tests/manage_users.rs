mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Ada (102) is an admin of company 100; she manages users once she is a
// Manager (position 5) there. Rita (100) is a sales rep of 100; Olga (106) is
// in company 101.

const ADA: &str = sessions::ADMIN;

async fn make_manager(pool: &MySqlPool) {
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (102, 5, 100)",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn only_managers_and_superusers_manage_users(pool: MySqlPool) {
    let server = server(pool.clone());
    for session in [ADA, sessions::REP, sessions::SUPER_ADMIN] {
        server
            .get("/v1/admin/users")
            .with_session(session)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    make_manager(&pool).await;
    let page: Value = server.get("/v1/admin/users").with_session(ADA).await.json();
    assert_eq!(page["company_id"], 100);
    assert_eq!(page["is_super_user"], false);
    let ids: Vec<i64> = page["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|user| user["id"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&100) && !ids.contains(&106) && !ids.contains(&105));
    assert_eq!(page["companies"].as_array().unwrap().len(), 1);
    assert_eq!(page["user_limits"][0]["company_id"], 100);
}

#[test_db::test(seed)]
async fn managers_add_users_with_positions(pool: MySqlPool) {
    make_manager(&pool).await;
    let server = server(pool.clone());
    let res = server
        .post("/v1/admin/users")
        .with_session(ADA)
        .json(&json!({
            "name": "Ian Installer",
            "email": " Ian@Acme.TEST ",
            "password": "secret1",
            "positions": [6, 9, 8],
            "company_id": 101,
        }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    let (email, company, hash, employee): (String, i32, String, bool) = sqlx::query_as(
        "SELECT email, company_id, password, is_employee <> 0 FROM users WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(email, "ian@acme.test");
    // Only superusers pick the company or grant Super Admin and check-in.
    assert_eq!(company, 100);
    assert!(bcrypt::verify("secret1", &hash).unwrap());
    assert!(employee);
    let positions: Vec<i32> =
        sqlx::query_scalar("SELECT position_id FROM users_positions WHERE user_id = ?")
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(positions, [6]);

    for (body, status) in [
        (
            json!({ "name": "X", "email": "ian@acme.test", "password": "secret" }),
            StatusCode::CONFLICT,
        ),
        (
            json!({ "name": "X", "email": "gone@acme.test", "password": "secret" }),
            StatusCode::CONFLICT,
        ),
        (
            json!({ "name": "X", "email": "x@acme.test", "password": "abc" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "name": "X", "email": "nope", "password": "secret" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "name": "X", "email": "y@acme.test", "password": "secret", "positions": [1, 4] }),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        server
            .post("/v1/admin/users")
            .with_session(ADA)
            .json(&body)
            .await
            .assert_status(status);
    }
}

#[test_db::test(seed)]
async fn managers_edit_users_and_keep_position_rows(pool: MySqlPool) {
    make_manager(&pool).await;
    let server = server(pool.clone());
    let form: Value = server
        .get("/v1/admin/users/100")
        .with_session(ADA)
        .await
        .json();
    assert_eq!(form["user"]["name"], "Rita Rep");
    assert_eq!(form["positions"], json!([1]));
    let kept: i64 = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM users_positions WHERE user_id = 100 AND position_id = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let res = server
        .put("/v1/admin/users/100")
        .with_session(ADA)
        .json(&json!({
            "name": "Rita R.",
            "email": "rita@acme.test",
            "company_id": 101,
            "positions": [1, 7],
            "marketing_company_ids": [100, 101],
            "cloudtalk_phone_number": "13175550111",
            "cloudtalk_agent_id": "42",
        }))
        .await;
    res.assert_status(StatusCode::NO_CONTENT);
    let (name, email, company, phone, agent): (String, String, i32, String, String) = sqlx::query_as(
        "SELECT name, email, company_id, cloudtalk_phone_number, cloudtalk_agent_id FROM users WHERE id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (
            name.as_str(),
            email.as_str(),
            company,
            phone.as_str(),
            agent.as_str()
        ),
        ("Rita R.", "rita@acme.test", 100, "3175550111", "42")
    );
    let rows: Vec<(i64, i32, i32)> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), position_id, company_id FROM users_positions WHERE user_id = 100 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    // The sales rep row stays; marketing only for her own company.
    assert_eq!(rows[0], (kept, 1, 100));
    assert_eq!(
        rows[1..].iter().map(|r| (r.1, r.2)).collect::<Vec<_>>(),
        [(7, 100)]
    );

    // A Super Admin is only for superusers to change; other companies are not hers.
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 9, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    server
        .put("/v1/admin/users/101")
        .with_session(ADA)
        .json(&json!({ "name": "Mark", "company_id": 100 }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/admin/users/106")
        .with_session(ADA)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put("/v1/admin/users/100")
        .with_session(ADA)
        .json(&json!({ "name": "Rita", "company_id": 100, "email": "rep@other.test" }))
        .await
        .assert_status(StatusCode::CONFLICT);
}

#[test_db::test(seed)]
async fn managers_delete_users_of_their_company(pool: MySqlPool) {
    make_manager(&pool).await;
    let server = server(pool.clone());
    server
        .delete("/v1/admin/users/106")
        .with_session(ADA)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete("/v1/admin/users/104")
        .with_session(ADA)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let deleted: bool = sqlx::query_scalar("SELECT is_deleted <> 0 FROM users WHERE id = 104")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(deleted);
}

/// A user's address cannot be a SQL pattern or an address list: mail and
/// notifications are matched to users by address.
#[test_db::test(seed)]
async fn user_addresses_are_single_plain_addresses(pool: MySqlPool) {
    make_manager(&pool).await;
    let server = server(pool.clone());
    for email in [
        "%@%.%",
        "ian%@acme.test",
        "Ian <ian@acme.test>",
        "a@acme.test,b@acme.test",
    ] {
        let res = server
            .post("/v1/admin/users")
            .with_session(ADA)
            .json(
                &json!({ "name": "Ian", "email": email, "password": "secret1", "positions": [1] }),
            )
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(
            res.json::<Value>()["error"],
            "Enter a valid email",
            "{email}"
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE name = 'Ian'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
