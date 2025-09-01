use axum::http::StatusCode;

pub const ERR_SEND_EMAIL: &str = "send_email_failed";
pub const ERR_SEND_TELEGRAM: &str = "telegram_send_failed";

pub const OK_RESPONSE: (StatusCode, &str) = (StatusCode::OK, "ok");
pub const CREATED_RESPONSE: (StatusCode, &str) = (StatusCode::CREATED, "created");
pub const FORBIDDEN_RESPONSE: (StatusCode, &'static str) = (StatusCode::FORBIDDEN, "forbidden");

pub const fn internal_error(error: &'static str) -> (StatusCode, &'static str) {
    (StatusCode::INTERNAL_SERVER_ERROR, error)
}
