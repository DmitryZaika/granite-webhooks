use dashmap::DashMap;
use sqlx::MySqlPool;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct VerificationState {
    pub email: String,
    pub code: String,
    pub attempts_left: u8,
}

#[derive(Clone)]
pub struct AppState {
    pub webhook_secret: String,
    pub bot: teloxide::Bot,
    pub pool: MySqlPool,
    pub verifications: Arc<DashMap<i64, VerificationState>>,
}
