//! Shared helpers for `api` integration tests. Every test gets a fresh database
//! with all migrations plus `api/seed/local_seed.sql` (see the seed for ids).
#![allow(dead_code)]

use api::app;
use api::auth::cookie::{SessionData, encode_session};
use api::state::AppState;
use api::storage::Storage;
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
    pub const DEALER: &str = "aaaaaaaa-0000-4000-8000-000000000107";
    pub const EXPIRED: &str = "bbbbbbbb-0000-4000-8000-0000000e0100";
    pub const LOGGED_OUT: &str = "bbbbbbbb-0000-4000-8000-0000000d0100";
    pub const TOO_OLD: &str = "bbbbbbbb-0000-4000-8000-0000000a0100";
    /// Company 20100: subscription plan with Quotes only.
    pub const QUOTES_ONLY_MANAGER: &str = "aaaaaaaa-0000-4000-8000-000000020100";
    pub const QUOTES_ONLY_REP: &str = "aaaaaaaa-0000-4000-8000-000000020101";
    /// Company 20101: subscription plan with a comp Signatures grant only.
    pub const COMP_SIGNATURES_MANAGER: &str = "aaaaaaaa-0000-4000-8000-000000020102";
    pub const COMP_SIGNATURES_REP: &str = "aaaaaaaa-0000-4000-8000-000000020103";
}

pub fn server(pool: MySqlPool) -> TestServer {
    TestServer::new(app(AppState::new(pool, SECRET, ALLOWED_ORIGIN)))
}

/// A server whose uploads go to an in-memory bucket the test can inspect.
pub fn server_with_storage(pool: MySqlPool) -> (TestServer, Storage) {
    let storage = Storage::memory();
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN).with_storage(storage.clone());
    (TestServer::new(app(state)), storage)
}

/// A PNG of the given size, as a browser would upload it.
pub fn png(width: u32, height: u32) -> Vec<u8> {
    let image = image::DynamicImage::new_rgb8(width, height);
    let mut out = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode png");
    out.into_inner()
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

/// Native signing verified for company 100
/// (`SIGNATURE_VERIFIED_COMPANY_SETTINGS`), for the signing tests. No test
/// needs it unset, so it stays set for the rest of the process.
pub fn verified_signature_settings() {
    static SETTINGS: std::sync::Once = std::sync::Once::new();
    SETTINGS.call_once(|| {
        let settings = serde_json::json!({ "100": {
            "verifiedAt": "2026-10-01T00:00:00Z",
            "evidenceReference": "ticket-1",
            "settings": {
                "parityVerified": true, "timezone": "America/Indiana/Indianapolis", "signatureMethods": ["type", "draw"],
                "allowDecline": true, "allowReassign": false, "authentication": "invitation", "reminders": false,
                "expiresInDays": 30, "notifications": { "viewed": false, "completed": false, "declined": false },
                "auditPage": true
            }
        }});
        // SAFETY: set once per process, to the same value whichever test gets
        // here first; the API reads it only through `std::env::var`.
        unsafe { std::env::set_var("SIGNATURE_VERIFIED_COMPANY_SETTINGS", settings.to_string()) };
    });
}
