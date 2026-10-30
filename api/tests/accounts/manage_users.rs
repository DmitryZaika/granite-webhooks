use crate::common;

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
            "email": " Ian@Acme.TEST ",
            "positions": [6, 9, 8],
            "company_id": 101,
        }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let created: Value = res.json();
    let id = created["id"].as_i64().unwrap();
    // This server sends no email: the manager gets the link to share.
    assert_eq!(created["email_sent"], false);
    let url = created["invite_url"].as_str().unwrap();
    let token = url.strip_prefix("http://localhost:5173/invite/").unwrap();
    assert_eq!(token.len(), 96);
    let (email, company, name, password, employee): (
        String,
        i32,
        Option<String>,
        Option<String>,
        bool,
    ) = sqlx::query_as(
        "SELECT email, company_id, name, password, is_employee <> 0 FROM users WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(email, "ian@acme.test");
    // Only superusers pick the company or grant Super Admin and check-in.
    assert_eq!(company, 100);
    // They set their own name and password from the invite.
    assert_eq!(name.as_deref(), Some(""));
    assert_eq!(password, None);
    assert!(employee);
    let positions: Vec<i32> =
        sqlx::query_scalar("SELECT position_id FROM users_positions WHERE user_id = ?")
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(positions, [6]);

    let page: Value = server.get("/v1/admin/users").with_session(ADA).await.json();
    let pending = |user_id: i64| {
        page["users"]
            .as_array()
            .unwrap()
            .iter()
            .find(|user| user["id"] == user_id)
            .unwrap()["invite_pending"]
            .clone()
    };
    assert_eq!(pending(id), true);
    assert_eq!(pending(100), false);

    for (body, status) in [
        (json!({ "email": "ian@acme.test" }), StatusCode::CONFLICT),
        (json!({ "email": "gone@acme.test" }), StatusCode::CONFLICT),
        (json!({ "email": "nope" }), StatusCode::BAD_REQUEST),
        (
            json!({ "email": "y@acme.test", "positions": [1, 4] }),
            StatusCode::BAD_REQUEST,
        ),
        // The manager no longer sets the password.
        (
            json!({ "email": "z@acme.test", "password": "secret" }),
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
async fn moving_a_rep_to_ringcentral_keeps_their_texts(pool: MySqlPool) {
    make_manager(&pool).await;
    sqlx::query(
        "UPDATE users SET cloudtalk_agent_id = '42', ringcentral_extension_id = '777'
          WHERE id = 100",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Her text, a reply to her line (no CloudTalk number saved: the one she
    // now has on RingCentral), and another rep's text.
    sqlx::query(
        "INSERT INTO cloudtalk_sms
           (recipient, text, direction, agent, sender_user_id, company_id,
            sender10, recipient10, phone_digits)
         VALUES (13175550199, 'Your slab is ready', 'outbound', '42', 100, 100,
                 '3175550111', '3175550199', '3175550199'),
                (13175550111, 'Thanks', 'inbound', NULL, NULL, 100,
                 '3175550199', '3175550111', '3175550199'),
                (13175550188, 'Not hers', 'outbound', '99', NULL, 100,
                 '3175550122', '3175550188', '3175550188')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());
    let save = || {
        server
            .put("/v1/admin/users/100")
            .with_session(ADA)
            .json(&json!({
                "name": "Rita Rep",
                "email": "rep@acme.test",
                "company_id": 100,
                "positions": [1],
                "ringcentral_phone_number": "3175550111",
            }))
    };
    save().await.assert_status(StatusCode::NO_CONTENT);
    save().await.assert_status(StatusCode::NO_CONTENT);
    // Hers, by agent id and by number, tagged with her extension; not the
    // other rep's.
    let copied: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT text, agent FROM ringcentral_sms
          WHERE company_id = 100 AND text IN ('Your slab is ready', 'Thanks', 'Not hers')
          ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        copied,
        [
            ("Your slab is ready".to_string(), Some("777".to_string())),
            ("Thanks".to_string(), Some("777".to_string())),
        ]
    );
    // And the five the seed has her sending; the second save copied nothing.
    let copies: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ringcentral_sms
          WHERE company_id = 100 AND idempotency_key LIKE 'cloudtalk:%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(copies, 7);
    // Her agent id is remembered after the save cleared it.
    let agents: Vec<String> = sqlx::query_scalar(
        "SELECT value FROM user_sms_identities
          WHERE user_id = 100 AND provider = 'cloudtalk' AND kind = 'agent'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(agents, ["42"]);
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
    let before = user_count(&pool).await;
    for email in [
        "%@%.%",
        "ian%@acme.test",
        "Ian <ian@acme.test>",
        "a@acme.test,b@acme.test",
    ] {
        let res = server
            .post("/v1/admin/users")
            .with_session(ADA)
            .json(&json!({ "email": email, "positions": [1] }))
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(
            res.json::<Value>()["error"],
            "Enter a valid email",
            "{email}"
        );
    }
    assert_eq!(user_count(&pool).await, before);
}

async fn user_count(pool: &MySqlPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn pending_invites_are_edited_and_sent_again(pool: MySqlPool) {
    make_manager(&pool).await;
    let server = server(pool.clone());
    let created: Value = server
        .post("/v1/admin/users")
        .with_session(ADA)
        .json(&json!({ "email": "ian@acme.test", "positions": [6] }))
        .await
        .json();
    let id = created["id"].as_i64().unwrap();
    let first = created["invite_url"].as_str().unwrap().to_string();
    let form: Value = server
        .get(&format!("/v1/admin/users/{id}"))
        .with_session(ADA)
        .await
        .json();
    assert_eq!(form["user"]["invite_pending"], true);

    // Positions change before they set up; their name stays theirs to fill.
    server
        .put(&format!("/v1/admin/users/{id}"))
        .with_session(ADA)
        .json(&json!({ "name": "", "email": "ian@acme.test", "company_id": 100, "positions": [1] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // Someone who set up their account keeps needing a name.
    server
        .put("/v1/admin/users/100")
        .with_session(ADA)
        .json(&json!({ "name": "", "company_id": 100, "positions": [1] }))
        .await
        .assert_status_bad_request();

    let res = server
        .post(&format!("/v1/admin/users/{id}/invite"))
        .with_session(ADA)
        .await;
    res.assert_status_ok();
    let second = res.json::<Value>()["invite_url"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(first, second);
    // The older link stops working.
    let path = |url: &str| {
        format!(
            "/v1/auth/invite/{}",
            url.rsplit('/').next().unwrap_or_default()
        )
    };
    server.get(&path(&first)).await.assert_status_not_found();
    let invite: Value = server.get(&path(&second)).await.json();
    assert_eq!(invite["email"], "ian@acme.test");
    assert_eq!(invite["company_name"], "Acme Stone");

    server
        .post("/v1/admin/users/100/invite")
        .with_session(ADA)
        .await
        .assert_status(StatusCode::CONFLICT);
    server
        .post("/v1/admin/users/106/invite")
        .with_session(ADA)
        .await
        .assert_status_not_found();
}
