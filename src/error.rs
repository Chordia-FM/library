//! Unified error → HTTP response (RFC 9457 problem+json).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    /// Missing/invalid/expired capability token, or TLS pin mismatch on a relay pull.
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("bad request: {0}")]
    BadRequest(String),
    /// Upstream/relay error talking to the Hub or a peer library.
    #[error("bad gateway: {0}")]
    BadGateway(String),
    #[error("too many requests")]
    TooManyRequests,
    #[error("not implemented")]
    NotImplemented,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type AppResult<T> = Result<T, AppError>;

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, type_uri, title) = match &self {
            AppError::NotFound => (
                StatusCode::NOT_FOUND,
                "urn:chordia:not-found",
                self.to_string(),
            ),
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "urn:chordia:unauthorized",
                self.to_string(),
            ),
            AppError::Forbidden => (
                StatusCode::FORBIDDEN,
                "urn:chordia:forbidden",
                self.to_string(),
            ),
            AppError::BadRequest(_) => (
                StatusCode::BAD_REQUEST,
                "urn:chordia:bad-request",
                self.to_string(),
            ),
            AppError::BadGateway(_) => (
                StatusCode::BAD_GATEWAY,
                "urn:chordia:bad-gateway",
                self.to_string(),
            ),
            AppError::TooManyRequests => (
                StatusCode::TOO_MANY_REQUESTS,
                "urn:chordia:too-many-requests",
                self.to_string(),
            ),
            AppError::NotImplemented => (
                StatusCode::NOT_IMPLEMENTED,
                "urn:chordia:not-implemented",
                self.to_string(),
            ),
            AppError::Internal(e) => {
                tracing::error!(error = ?e, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "urn:chordia:internal",
                    "internal server error".to_string(),
                )
            }
        };

        (
            status,
            Json(json!({
                "type":   type_uri,
                "status": status.as_u16(),
                "title":  title,
            })),
        )
            .into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::RowNotFound => AppError::NotFound,
            other => AppError::Internal(other.into()),
        }
    }
}
