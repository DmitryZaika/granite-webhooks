use axum::http::StatusCode;

pub const ERR_SEND_EMAIL: &str = "send_email_failed";
pub const ERR_SEND_TELEGRAM: &str = "telegram_send_failed";
pub const ERR_DB: &str = "database_failed";

pub const OK_RESPONSE: (StatusCode, &str) = (StatusCode::OK, "ok");
pub const CREATED_RESPONSE: (StatusCode, &str) = (StatusCode::CREATED, "created");
pub const ACCEPTED_RESPONSE: (StatusCode, &str) = (StatusCode::ACCEPTED, "accepted");
pub const FORBIDDEN_RESPONSE: (StatusCode, &str) = (StatusCode::FORBIDDEN, "forbidden");
pub const MALFORMED_RESPONSE: (StatusCode, &str) = (StatusCode::UNPROCESSABLE_ENTITY, "malformed");
pub const BAD_REQUEST: (StatusCode, &str) = (StatusCode::BAD_REQUEST, "bad_request");

pub const fn internal_error(error: &'static str) -> (StatusCode, &'static str) {
    (StatusCode::INTERNAL_SERVER_ERROR, error)
}

pub const SALES_WORKER: Option<i32> = Some(1);
pub const SALES_MANAGER: Option<i32> = Some(2);
