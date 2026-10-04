//! Shared helpers for `api` integration tests. Every test gets a fresh database
//! with all migrations plus `api/seed/local_seed.sql` (see the seed for ids).
#![allow(dead_code)]

use api::app;
use api::auth::cookie::{SessionData, encode_session};
use api::state::AppState;
use axum_test::{TestRequest, TestServer};
use sqlx::MySqlPool;

pub const SECRET: &str = "test-secret";
pub const ALLOWED_ORIGIN: &str = "http://localhost:5173";

pub mod sessions {
    pub const REP: &str = "aaaaaaaa-0000-4000-8000-000000000100";
    pub const MANAGER: &str = "aaaaaaaa-0000-4000-8000-000000000101";
    pub const ADMIN: &str = "aaaaaaaa-0000-4000-8000-000000000102";
    pub const SUPER_ADMIN: &str = "aaaaaaaa-0000-4000-8000-000000000103";
    pub const VIEWER: &str = "aaaaaaaa-0000-4000-8000-000000000104";
    pub const DELETED_USER: &str = "aaaaaaaa-0000-4000-8000-000000000105";
    pub const OTHER_COMPANY_REP: &str = "aaaaaaaa-0000-4000-8000-000000000106";
    pub const EXPIRED: &str = "bbbbbbbb-0000-4000-8000-0000000e0100";
    pub const LOGGED_OUT: &str = "bbbbbbbb-0000-4000-8000-0000000d0100";
    pub const TOO_OLD: &str = "bbbbbbbb-0000-4000-8000-0000000a0100";
}

pub fn server(pool: MySqlPool) -> TestServer {
    TestServer::new(app(AppState::new(pool, SECRET, ALLOWED_ORIGIN)))
}

/// `Cookie` header value exactly as the Remix app would have set it.
pub fn cookie(session_id: &str) -> String {
    cookie_for(SessionData {
        session_id: Some(session_id.to_string()),
        active_company_id: None,
    })
}

pub fn cookie_for(data: SessionData) -> String {
    format!("__session={}", encode_session(&data, SECRET))
}

pub trait WithSession {
    fn with_session(self, session_id: &str) -> Self;
}

impl WithSession for TestRequest {
    fn with_session(self, session_id: &str) -> Self {
        self.add_header("cookie", cookie(session_id))
    }
}

pub fn ids(rows: &serde_json::Value) -> Vec<i64> {
    rows.as_array()
        .expect("array response")
        .iter()
        .map(|row| row["id"].as_i64().expect("row id"))
        .collect()
}
