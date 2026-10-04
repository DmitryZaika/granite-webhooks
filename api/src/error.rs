use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use lambda_http::tracing;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// No valid session: the frontend sends the user to `/login`.
    #[error("unauthorized")]
    Unauthorized,
    /// Valid session, but the user lacks the permission the route needs.
    #[error("forbidden")]
    Forbidden,
    #[error("{0}")]
    BadRequest(String),
    #[error("database error")]
    Database(#[from] sqlx::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Database(err) => {
                tracing::error!(?err, "database error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
