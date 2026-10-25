//! Agent tokens: the claude-job-runner's agent reads the API as the person who
//! asked, read-only, in the one company the question was asked in.

mod common;

use api::app;
use api::auth::agent::{Claims, issue};
use api::auth::cookie::SessionData;
use api::state::AppState;
use axum::http::StatusCode;
use axum_test::{TestRequest, TestServer};
use common::{ALLOWED_ORIGIN, SECRET, WithSession, cookie_for, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const RUNNER_KEY: &str = "runner-key-runner-key-runner-key-0123";

fn server(pool: MySqlPool) -> TestServer {
    TestServer::new(app(
        AppState::new(pool, SECRET, ALLOWED_ORIGIN).with_agent_runner_key(RUNNER_KEY)
    ))
}

trait WithBearer {
    fn bearer(self, token: &str) -> Self;
}

impl WithBearer for TestRequest {
    fn bearer(self, token: &str) -> Self {
        self.add_header("authorization", format!("Bearer {token}"))
    }
}

/// Queues a question as the session's user; its id.
async fn ask(server: &TestServer, session: &str) -> i64 {
    let response = server
        .post("/v1/ai/agentic-questions")
        .with_session(session)
        .json(&json!({ "query": "What did we text Alice?", "new_chat": true }))
        .await;
    response.assert_status(StatusCode::CREATED);
    response.json::<Value>()["id"].as_i64().unwrap()
}

/// The runner's token for question `id`.
async fn token(server: &TestServer, id: i64) -> String {
    let response = server
        .post(&format!("/v1/ai/agentic-questions/{id}/agent-token"))
        .bearer(RUNNER_KEY)
        .await;
    response.assert_status(StatusCode::CREATED);
    response.json::<Value>()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn set_status(pool: &MySqlPool, id: i64, status: &str) {
    sqlx::query("UPDATE chat_messages SET status = ? WHERE id = ?")
        .bind(status)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

#[test_db::test(seed)]
async fn only_the_runner_key_gets_a_token(pool: MySqlPool) {
    let server = server(pool.clone());
    let id = ask(&server, sessions::REP).await;
    let path = format!("/v1/ai/agentic-questions/{id}/agent-token");
    server
        .post(&path)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post(&path)
        .bearer("runner-key-runner-key-runner-key-0124")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post(&path)
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    let agent = token(&server, id).await;
    server
        .post(&path)
        .bearer(&agent)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    // No key configured: nobody gets a token.
    let keyless = common::server(pool);
    keyless
        .post(&path)
        .bearer(RUNNER_KEY)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn a_token_reads_exactly_what_the_asker_sees(pool: MySqlPool) {
    let server = server(pool.clone());
    let id = ask(&server, sessions::REP).await;
    let agent = token(&server, id).await;
    let me: Value = server.get("/v1/me").bearer(&agent).await.json();
    assert_eq!(
        (me["id"].as_i64(), me["company_id"].as_i64()),
        (Some(100), Some(100))
    );

    // A rep's token gets the rep's own view even when asking for everyone's;
    // an admin's gets the admin's.
    for session in [sessions::REP, sessions::ADMIN] {
        let id = ask(&server, session).await;
        let agent = token(&server, id).await;
        for path in [
            "/v1/me",
            "/v1/customers/1000",
            "/v1/customers/search?term=Alice",
            "/v1/sms/cloudtalk/threads?scope=all",
            "/v1/sms/all/threads/3175550100?scope=all",
            "/v1/emails/threads?scope=admin",
        ] {
            let as_user = server.get(path).with_session(session).await;
            let as_agent = server.get(path).bearer(&agent).await;
            assert_eq!(
                as_agent.status_code(),
                as_user.status_code(),
                "{session} {path}"
            );
            assert_eq!(as_agent.text(), as_user.text(), "{session} {path}");
        }
    }
}

#[test_db::test(seed)]
async fn a_token_never_sees_another_company(pool: MySqlPool) {
    let server = server(pool.clone());
    let id = ask(&server, sessions::REP).await;
    let agent = token(&server, id).await;
    server
        .get("/v1/customers/1000")
        .bearer(&agent)
        .await
        .assert_status_ok();
    server
        .get("/v1/customers/2000")
        .bearer(&agent)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let found: Value = server
        .get("/v1/customers/search?term=Other")
        .bearer(&agent)
        .await
        .json();
    assert!(
        found
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["id"].as_i64() != Some(2000)),
        "{found}"
    );
    // Routes that name a company only take the token's.
    server
        .get("/v1/marketing/101/customers/search?term=Other")
        .bearer(&agent)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // Claims naming another company do not verify against the question.
    let forged = issue(
        SECRET,
        &Claims {
            q: id,
            u: 100,
            c: 101,
            exp: chrono::Utc::now().timestamp() + 60,
        },
    );
    server
        .get("/v1/me")
        .bearer(&forged)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn a_super_admin_stays_in_the_company_they_asked_in(pool: MySqlPool) {
    let server = server(pool.clone());
    // Sam's own company is 101; he asks while acting in 100.
    let in_100 = cookie_for(SessionData {
        session_id: Some(sessions::SUPER_ADMIN.into()),
        active_company_id: Some(100),
    });
    let response = server
        .post("/v1/ai/agentic-questions")
        .add_header("cookie", in_100)
        .json(&json!({ "query": "Q", "new_chat": true }))
        .await;
    response.assert_status(StatusCode::CREATED);
    let id = response.json::<Value>()["id"].as_i64().unwrap();
    let agent = token(&server, id).await;
    let me: Value = server.get("/v1/me").bearer(&agent).await.json();
    assert_eq!(
        (me["id"].as_i64(), me["company_id"].as_i64()),
        (Some(103), Some(100))
    );
    server
        .get("/v1/customers/1000")
        .bearer(&agent)
        .await
        .assert_status_ok();
    server
        .get("/v1/customers/2000")
        .bearer(&agent)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn a_token_is_never_a_superuser(pool: MySqlPool) {
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(pool.clone());
    server
        .get("/v1/superuser/companies")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
    let id = ask(&server, sessions::ADMIN).await;
    let agent = token(&server, id).await;
    let me: Value = server.get("/v1/me").bearer(&agent).await.json();
    assert_eq!(me["is_superuser"], false);
    assert_eq!(me["is_admin"], true);
    server
        .get("/v1/superuser/companies")
        .bearer(&agent)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn a_token_only_reads_and_transcribes(pool: MySqlPool) {
    let server = server(pool.clone());
    let id = ask(&server, sessions::REP).await;
    let agent = token(&server, id).await;
    server
        .post("/v1/me/todos")
        .bearer(&agent)
        .json(&json!({ "rich_text": "x" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .put("/v1/me/sidebar-pin")
        .bearer(&agent)
        .json(&json!({ "pinned": true }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .delete("/v1/me/todos/done")
        .bearer(&agent)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let todos: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todolist WHERE rich_text = 'x'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(todos, 0);
    // Transcribing gets past the check: the bad id is the handler's 400.
    server
        .post("/v1/call-transcripts")
        .bearer(&agent)
        .json(&json!({ "call_id": "not a call id!" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn a_token_dies_with_its_question(pool: MySqlPool) {
    let server = server(pool.clone());
    let id = ask(&server, sessions::REP).await;
    let agent = token(&server, id).await;
    set_status(&pool, id, "pending").await;
    server.get("/v1/me").bearer(&agent).await.assert_status_ok();
    set_status(&pool, id, "done").await;
    server
        .get("/v1/me")
        .bearer(&agent)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post(&format!("/v1/ai/agentic-questions/{id}/agent-token"))
        .bearer(RUNNER_KEY)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    let other = ask(&server, sessions::REP).await;
    let agent = token(&server, other).await;
    set_status(&pool, other, "failed").await;
    server
        .get("/v1/me")
        .bearer(&agent)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn expired_and_altered_tokens_are_refused(pool: MySqlPool) {
    let server = server(pool.clone());
    let id = ask(&server, sessions::REP).await;
    let claims = Claims {
        q: id,
        u: 100,
        c: 100,
        exp: chrono::Utc::now().timestamp() - 1,
    };
    server
        .get("/v1/me")
        .bearer(&issue(SECRET, &claims))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    let live = Claims {
        exp: chrono::Utc::now().timestamp() + 60,
        ..claims
    };
    server
        .get("/v1/me")
        .bearer(&issue("another secret", &live))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    let agent = token(&server, id).await;
    server
        .get("/v1/me")
        .bearer(&format!("{agent}x"))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn stale_answered_or_foreign_questions_get_no_token(pool: MySqlPool) {
    let server = server(pool.clone());
    let mint = |id: i64| {
        server
            .post(&format!("/v1/ai/agentic-questions/{id}/agent-token"))
            .bearer(RUNNER_KEY)
    };
    mint(999_999).await.assert_status(StatusCode::NOT_FOUND);

    let old = ask(&server, sessions::REP).await;
    sqlx::query("UPDATE chat_messages SET created_at = DATE_SUB(CURRENT_TIMESTAMP, INTERVAL 8 DAY) WHERE id = ?")
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();
    mint(old).await.assert_status(StatusCode::NOT_FOUND);

    let plain = ask(&server, sessions::REP).await;
    sqlx::query("UPDATE chat_messages SET is_agentic = NULL WHERE id = ?")
        .bind(plain)
        .execute(&pool)
        .await
        .unwrap();
    mint(plain).await.assert_status(StatusCode::NOT_FOUND);

    // A deleted asker gets no new token, and an issued one stops working.
    let id = ask(&server, sessions::REP).await;
    let agent = token(&server, id).await;
    sqlx::query("UPDATE users SET is_deleted = 1 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    mint(id).await.assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/me")
        .bearer(&agent)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}
