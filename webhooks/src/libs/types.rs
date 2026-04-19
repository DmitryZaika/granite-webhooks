use axum::http::StatusCode;

pub type BasicResponse = (StatusCode, &'static str);
