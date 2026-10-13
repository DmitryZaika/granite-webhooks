use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use lambda_http::tracing;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// No valid session: the frontend sends the user to `/login`.
    #[error("unauthorized")]
    Unauthorized,
    /// Valid session, but the user lacks the permission the route needs.
    #[error("forbidden")]
    Forbidden,
    /// The resource does not exist, or belongs to another company.
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    BadRequest(String),
    /// The request conflicts with the data, e.g. deleting something in use.
    #[error("{0}")]
    Conflict(String),
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Storage(#[from] crate::storage::StorageError),
    /// A service the route depends on failed (e.g. sending mail).
    #[error("{0}")]
    Upstream(String),
    /// The work was partly done and could not be finished.
    #[error("{0}")]
    Internal(String),
    /// Wrong credentials (sign-in), with the reason for the user.
    #[error("{0}")]
    Unauthenticated(String),
    /// Too many attempts; try again later.
    #[error("{0}")]
    TooManyRequests(String),
    /// A service the route depends on is not available right now.
    #[error("{0}")]
    Unavailable(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Unauthorized | Self::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Storage(crate::storage::StorageError::Disabled) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::Storage(err) => {
                tracing::error!(?err, "storage error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
            Self::Database(err) => {
                tracing::error!(?err, "database error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        let body = ErrorBody {
            error: self.to_string(),
        };
        (status, Json(body)).into_response()
    }
}

/// Body of every error response.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    /// Short reason: `unauthorized`, `forbidden`, `not found`, `database error`, or the
    /// bad-request message (e.g. `invalid sales_rep: abc`).
    pub error: String,
}

pub type ApiResult<T> = Result<T, ApiError>;
