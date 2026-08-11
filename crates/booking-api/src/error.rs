//! The application error type and its HTTP representation.
//!
//! Every handler in this service returns `Result<T, AppError>`. Because `AppError`
//! implements `IntoResponse`, you can use `?` freely in handlers and get a consistent JSON
//! error body for free. Add variants as you need them.
//!
//! The shape on the wire is always:
//!
//! ```json
//! { "error": { "code": "SEAT_UNAVAILABLE", "message": "seat E7 is already booked" } }
//! ```

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

// Most variants are unconstructed until you write handlers. Drop the allow when they are
// all in use — an unused error variant is otherwise a useful smell.
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The request was understood but the referenced thing does not exist.
    #[error("{0} not found")]
    NotFound(String),

    /// The request is well-formed but conflicts with current state.
    #[error("{0}")]
    Conflict(String),

    /// The request itself is wrong: unknown seat id, empty seat list, a show in the past.
    #[error("{0}")]
    Validation(String),

    /// The payment gateway misbehaved or is unreachable. Worth keeping separate from
    /// `Internal`: what you tell the caller when a downstream dependency is down is a real
    /// decision.
    #[error("payment gateway error: {0}")]
    Upstream(String),

    #[error(transparent)]
    Database(#[from] sqlx::Error),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    /// Escape hatch for genuine bugs. The message is logged, never returned.
    #[error("internal error: {0}")]
    Internal(String),
}

// These are reached only through the `IntoResponse` impl below, which nothing calls until
// you add a handler. Drop the allow once you have one.
#[allow(dead_code)]
impl AppError {
    fn code(&self) -> &'static str {
        match self {
            AppError::NotFound(_) => "NOT_FOUND",
            AppError::Conflict(_) => "CONFLICT",
            AppError::Validation(_) => "VALIDATION_FAILED",
            AppError::Upstream(_) | AppError::Http(_) => "UPSTREAM_UNAVAILABLE",
            AppError::Database(_) | AppError::Internal(_) => "INTERNAL_ERROR",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            AppError::Upstream(_) | AppError::Http(_) => StatusCode::BAD_GATEWAY,
            AppError::Database(_) | AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Whether the underlying cause is safe to show a client.
    ///
    /// Database and internal errors leak schema details and are replaced with a generic
    /// message; the real cause goes to the log instead.
    fn is_public(&self) -> bool {
        !matches!(
            self,
            AppError::Database(_) | AppError::Internal(_) | AppError::Http(_)
        )
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();

        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        } else {
            tracing::debug!(error = %self, "request rejected");
        }

        let message = if self.is_public() {
            self.to_string()
        } else {
            "something went wrong on our side".to_owned()
        };

        let body = Json(json!({
            "error": { "code": self.code(), "message": message }
        }));

        (status, body).into_response()
    }
}

/// Convenience alias so handler signatures stay short.
pub type AppResult<T> = Result<T, AppError>;
