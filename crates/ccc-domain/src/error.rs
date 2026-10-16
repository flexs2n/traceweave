//! Error taxonomy (TIS §13.2).
//!
//! Every error carries a stable machine-readable code, a human-readable
//! message, and an optional request identifier. Messages must never contain
//! credentials, secret values, or private repository content.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable machine-readable error codes (TIS §13.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidRequest,
    RepositoryNotFound,
    SnapshotNotFound,
    PermissionDenied,
    IndexStale,
    UnsupportedLanguage,
    BudgetExceeded,
    ResourceLimitExceeded,
    ExecutionNotAuthorized,
    JobConflict,
    DependencyUnavailable,
    InternalError,
}

impl ErrorCode {
    /// The canonical string form used in CLI/API output.
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::InvalidRequest => "INVALID_REQUEST",
            ErrorCode::RepositoryNotFound => "REPOSITORY_NOT_FOUND",
            ErrorCode::SnapshotNotFound => "SNAPSHOT_NOT_FOUND",
            ErrorCode::PermissionDenied => "PERMISSION_DENIED",
            ErrorCode::IndexStale => "INDEX_STALE",
            ErrorCode::UnsupportedLanguage => "UNSUPPORTED_LANGUAGE",
            ErrorCode::BudgetExceeded => "BUDGET_EXCEEDED",
            ErrorCode::ResourceLimitExceeded => "RESOURCE_LIMIT_EXCEEDED",
            ErrorCode::ExecutionNotAuthorized => "EXECUTION_NOT_AUTHORIZED",
            ErrorCode::JobConflict => "JOB_CONFLICT",
            ErrorCode::DependencyUnavailable => "DEPENDENCY_UNAVAILABLE",
            ErrorCode::InternalError => "INTERNAL_ERROR",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A structured error with a stable code, message, and request id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CccError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl CccError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            request_id: None,
        }
    }

    /// Attaches a request identifier for correlation (TIS §13.2).
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, message)
    }

    pub fn repository_not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::RepositoryNotFound, message)
    }

    pub fn snapshot_not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::SnapshotNotFound, message)
    }

    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::PermissionDenied, message)
    }

    pub fn index_stale(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::IndexStale, message)
    }

    pub fn unsupported_language(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::UnsupportedLanguage, message)
    }

    pub fn budget_exceeded(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BudgetExceeded, message)
    }

    pub fn resource_limit_exceeded(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ResourceLimitExceeded, message)
    }

    pub fn execution_not_authorized(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ExecutionNotAuthorized, message)
    }

    pub fn job_conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::JobConflict, message)
    }

    pub fn dependency_unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::DependencyUnavailable, message)
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InternalError, message)
    }
}

impl fmt::Display for CccError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)?;
        if let Some(request_id) = &self.request_id {
            write!(f, " (request_id={request_id})")?;
        }
        Ok(())
    }
}

impl std::error::Error for CccError {}

impl From<uuid::Error> for CccError {
    fn from(err: uuid::Error) -> Self {
        CccError::invalid_request(format!("invalid identifier: {err}"))
    }
}

impl From<std::io::Error> for CccError {
    fn from(err: std::io::Error) -> Self {
        CccError::internal_error(format!("io failure: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_serialize_screaming_snake() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::InvalidRequest).unwrap(),
            "\"INVALID_REQUEST\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::ExecutionNotAuthorized).unwrap(),
            "\"EXECUTION_NOT_AUTHORIZED\""
        );
    }

    #[test]
    fn error_round_trips_with_request_id() {
        let err = CccError::index_stale("active generation does not match snapshot")
            .with_request_id("req-1");
        let json = serde_json::to_string(&err).unwrap();
        let back: CccError = serde_json::from_str(&json).unwrap();
        assert_eq!(back, err);
        assert_eq!(back.code.as_str(), "INDEX_STALE");
        assert_eq!(back.request_id.as_deref(), Some("req-1"));
    }

    #[test]
    fn request_id_omitted_when_absent() {
        let err = CccError::invalid_request("nope");
        let json = serde_json::to_string(&err).unwrap();
        assert!(!json.contains("request_id"));
    }
}
