//! The error taxonomy. Every failure the domain can produce is one of these, and each maps to a
//! transport response in exactly one place.
//!
//! This replaces the pattern where failures were bare errors classified at each call site, which
//! let a new endpoint invent a new error shape by accident.

use std::fmt;

pub type Result<T> = std::result::Result<T, DomainError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The caller could fix this by sending something else.
    Validation,
    /// It does not exist, or the caller may not know whether it exists.
    NotFound,
    /// Authenticated but not permitted.
    Forbidden,
    /// Cannot be applied against current state, e.g. an already-superseded target.
    Conflict,
    /// A dependency is saturated or down. Distinct from internal: retrying may work.
    Unavailable,
    /// Ours. The message never reaches a client.
    Internal,
}

impl Kind {
    pub fn http_status(self) -> u16 {
        match self {
            Kind::Validation => 400,
            Kind::NotFound => 404,
            Kind::Forbidden => 403,
            Kind::Conflict => 409,
            Kind::Unavailable => 503,
            Kind::Internal => 500,
        }
    }
}

#[derive(Debug)]
pub struct DomainError {
    pub kind: Kind,
    message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    /// Set at the raise site so a transport publishes a stable string. No transport matches on
    /// message text, which changes whenever the wording improves.
    code: Option<&'static str>,
}

impl DomainError {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), source: None, code: None }
    }

    pub fn with_code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }

    pub fn code(&self) -> Option<&'static str> {
        self.code
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(Kind::Validation, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Kind::NotFound, message)
    }
    /// Messages here must not leak the shape of what was refused: naming a namespace a client
    /// cannot read tells it that namespace exists.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(Kind::Forbidden, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(Kind::Conflict, message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(Kind::Unavailable, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(Kind::Internal, message)
    }

    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// What a client is allowed to see. Internal errors never expose their message.
    pub fn client_message(&self) -> &str {
        match self.kind {
            Kind::Internal => "internal error",
            _ => &self.message,
        }
    }

    /// The full message, for logs only.
    pub fn log_message(&self) -> String {
        match &self.source {
            Some(s) => format!("{}: {s}", self.message),
            None => self.message.clone(),
        }
    }
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DomainError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|s| s.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// A database failure is ours, not the caller's, so it becomes Internal and the detail stays in
/// the log. The one exception worth making later is a unique-violation, which is a Conflict.
impl From<sqlx::Error> for DomainError {
    fn from(e: sqlx::Error) -> Self {
        DomainError::internal("database error").with_source(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_each_kind_to_a_status() {
        assert_eq!(Kind::Validation.http_status(), 400);
        assert_eq!(Kind::Forbidden.http_status(), 403);
        assert_eq!(Kind::NotFound.http_status(), 404);
        assert_eq!(Kind::Conflict.http_status(), 409);
        assert_eq!(Kind::Unavailable.http_status(), 503);
        assert_eq!(Kind::Internal.http_status(), 500);
    }

    #[test]
    fn internal_errors_never_reach_the_client() {
        let e = DomainError::internal("connection string is postgres://user:hunter2@host/db");
        assert_eq!(e.client_message(), "internal error");
        assert!(e.log_message().contains("hunter2"), "the log keeps the detail");
    }

    #[test]
    fn other_kinds_are_shown_verbatim() {
        let e = DomainError::validation("content cannot be empty");
        assert_eq!(e.client_message(), "content cannot be empty");
    }
}
