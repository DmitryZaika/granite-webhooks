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
    /// Like `Forbidden`, saying what to turn on and where.
    #[error("{0}")]
    Denied(String),
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
    /// The company's access is paused until it pays (`/admin/billing`).
    #[error("Payment required")]
    PaymentRequired,
    /// The company's plan does not include this add-on.
    #[error("{} is an add-on for your plan.", .0.label())]
    AddonRequired(crate::entitlements::AddonKey),
    /// `{"error": "...", "code": "...", ...extra}` with any status: errors a
    /// page tells apart by `code` (the public signup flow).
    #[error("{}", .0.error)]
    Coded(Box<CodedError>),
}

/// The body of [`ApiError::Coded`].
#[derive(Debug, Clone)]
pub struct CodedError {
    pub status: StatusCode,
    /// A sentence the page can show as is.
    pub error: String,
    /// Machine-readable, e.g. `email_taken`.
    pub code: &'static str,
    /// More fields merged into the body, e.g. `{"attempts_left": 3}`.
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ApiError {
    pub fn coded(status: StatusCode, code: &'static str, error: impl Into<String>) -> Self {
        Self::Coded(Box::new(CodedError {
            status,
            error: error.into(),
            code,
            extra: serde_json::Map::new(),
        }))
    }

    /// Adds a field to a [`ApiError::Coded`] body; other errors are unchanged.
    #[must_use]
    pub fn with_field(mut self, name: &str, value: impl Into<serde_json::Value>) -> Self {
        if let Self::Coded(coded) = &mut self {
            coded.extra.insert(name.to_string(), value.into());
        }
        self
    }
}

/// What actually failed, kept on the response for the 5xx reporting in
/// `lib.rs` (the body only says e.g. `database error`).
#[derive(Clone, Debug)]
pub struct ErrorDetail {
    /// `ApiError` or `Panic`: the exception type in `PostHog`.
    pub kind: &'static str,
    pub message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let detail = ErrorDetail {
            kind: "ApiError",
            message: format!("{self:?}"),
        };
        let status = match &self {
            Self::Unauthorized | Self::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden | Self::Denied(_) => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::PaymentRequired => {
                let body = PaymentRequiredBody {
                    error: self.to_string(),
                    code: PAYMENT_REQUIRED_CODE.into(),
                };
                let mut response = (StatusCode::PAYMENT_REQUIRED, Json(body)).into_response();
                response.extensions_mut().insert(detail);
                return response;
            }
            Self::AddonRequired(key) => {
                let body = AddonRequiredBody {
                    error: self.to_string(),
                    addon: Some(key.as_str().into()),
                };
                let mut response = (StatusCode::FORBIDDEN, Json(body)).into_response();
                response.extensions_mut().insert(detail);
                return response;
            }
            Self::Coded(coded) => {
                let mut body = coded.extra.clone();
                body.insert("error".into(), coded.error.clone().into());
                body.insert("code".into(), coded.code.into());
                let mut response =
                    (coded.status, Json(serde_json::Value::Object(body))).into_response();
                response.extensions_mut().insert(detail);
                return response;
            }
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
        let mut response = (status, Json(body)).into_response();
        response.extensions_mut().insert(detail);
        response
    }
}

/// Body of every error response.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    /// Short reason: `unauthorized`, `forbidden`, `not found`, `database error`, or the
    /// bad-request message (e.g. `invalid sales_rep: abc`).
    pub error: String,
}

pub const PAYMENT_REQUIRED_CODE: &str = "payment_required";

/// Body of a 402: the company's access is paused until it pays.
#[derive(Debug, Serialize, ToSchema)]
pub struct PaymentRequiredBody {
    /// `Payment required`.
    pub error: String,
    /// Always `payment_required`: open `/admin/billing`.
    pub code: String,
}

/// Body of a 403 that may be about the plan: `addon` names the add-on the
/// company needs (`signatures`, `quotes` or `quickbooks`) and is absent when
/// the caller lacks a permission instead.
/// An error the page tells apart by `code`; some codes add fields
/// (`attempts_left`, `retry_after_seconds`).
#[derive(Debug, Serialize, ToSchema)]
pub struct CodedErrorBody {
    /// A sentence the page can show as is.
    pub error: String,
    /// Machine-readable reason, e.g. `email_taken`, `invalid_code`.
    pub code: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AddonRequiredBody {
    /// E.g. `Quotes is an add-on for your plan.`, or `forbidden`.
    pub error: String,
    /// The add-on to add to the plan; absent for other refusals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addon: Option<String>,
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_response_keeps_what_failed_for_reporting() {
        let response = ApiError::Internal("quote half saved".into()).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let detail = response.extensions().get::<ErrorDetail>().expect("detail");
        assert_eq!(detail.kind, "ApiError");
        assert_eq!(detail.message, r#"Internal("quote half saved")"#);
    }

    #[tokio::test]
    async fn a_missing_addon_is_a_403_naming_it() {
        let response =
            ApiError::AddonRequired(crate::entitlements::AddonKey::Quotes).into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("body");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).expect("json"),
            serde_json::json!({"error": "Quotes is an add-on for your plan.", "addon": "quotes"})
        );
    }
}
