use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("{code}: {message}")]
pub struct ServerError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ServerError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            message: message.into(),
        }
    }

    pub fn validation_error(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "validation_error",
            message: message.into(),
        }
    }

    pub fn hash_mismatch(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "hash_mismatch",
            message: message.into(),
        }
    }

    pub fn visibility_mismatch(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "visibility_mismatch",
            message: message.into(),
        }
    }

    pub fn unauthenticated(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthenticated",
            message: message.into(),
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "forbidden",
            message: message.into(),
        }
    }

    pub fn skill_not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "skill_not_found",
            message: message.into(),
        }
    }

    pub fn team_not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "team_not_found",
            message: message.into(),
        }
    }

    pub fn stack_not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "stack_not_found",
            message: message.into(),
        }
    }

    pub fn audit_event_not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "audit_event_not_found",
            message: message.into(),
        }
    }

    pub fn version_not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "version_not_found",
            message: message.into(),
        }
    }

    pub fn no_current_version(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "no_current_version",
            message: message.into(),
        }
    }

    pub fn stack_resolution_failed(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "stack_resolution_failed",
            message: message.into(),
        }
    }

    pub fn already_yanked(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "already_yanked",
            message: message.into(),
        }
    }

    pub fn already_deprecated(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "already_deprecated",
            message: message.into(),
        }
    }

    pub fn version_yanked(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::GONE,
            code: "version_yanked",
            message: message.into(),
        }
    }

    pub fn quota_exceeded(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "quota_exceeded",
            message: message.into(),
        }
    }

    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: "payload_too_large",
            message: message.into(),
        }
    }

    pub fn internal_error() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: "internal server error".to_string(),
        }
    }

    pub fn audit_failed(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "audit_failed",
            message: message.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

/// Map a database failure to an opaque 500, logging the detail server-side only.
pub(crate) fn map_sql(err: sqlx::Error) -> ServerError {
    tracing::error!(error = %err, "database operation failed");
    ServerError::internal_error()
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        let status = self.status;
        let body = ErrorEnvelope {
            error: ErrorBody {
                code: self.code,
                message: self.message,
                http_status: status.as_u16(),
            },
        };
        (status, Json(body)).into_response()
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
    http_status: u16,
}
