use sqlx::MySqlPool;

#[derive(Clone)]
pub struct AppState {
    pub webhook_secret: String,
    pub bot: teloxide::Bot,
    pub pool: MySqlPool,
}
