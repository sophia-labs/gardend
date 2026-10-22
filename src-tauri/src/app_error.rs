use std::{error::Error, fmt};

pub(crate) type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppErrorKind {
    Validation,
    NotFound,
    Conflict,
    Capacity,
    Deadline,
    Storage,
    Database,
    Serialization,
    Rdf,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppError {
    kind: AppErrorKind,
    message: String,
    code: Option<&'static str>,
}

impl AppError {
    pub(crate) fn validation(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Validation, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::NotFound, message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Conflict, message)
    }

    pub(crate) fn capacity(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Capacity, message)
    }

    pub(crate) fn deadline(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Deadline, message)
    }

    pub(crate) fn storage(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Storage, message)
    }

    pub(crate) fn database(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Database, message)
    }

    pub(crate) fn serialization(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Serialization, message)
    }

    pub(crate) fn rdf(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Rdf, message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Internal, message)
    }

    pub(crate) fn kind(&self) -> AppErrorKind {
        self.kind
    }

    pub(crate) fn message_ref(&self) -> &str {
        &self.message
    }

    pub(crate) fn message(self) -> String {
        self.message
    }

    /// Attach a taxonomy code (`app_error_codes`). Chained onto an existing
    /// constructor so no call site changes shape:
    /// `AppError::conflict(msg).with_code(codes::X)`.
    pub(crate) fn with_code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }

    pub(crate) fn code(&self) -> Option<&'static str> {
        self.code
    }

    /// Rewrite the message, preserving both kind and code. Use this instead
    /// of reconstructing an `AppError` from a re-matched kind, which would
    /// otherwise drop the code silently.
    pub(crate) fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    fn new(kind: AppErrorKind, message: impl Into<String>) -> Self {
        let message = message.into();
        if message.starts_with("source_body_unavailable:") {
            return Self {
                kind: AppErrorKind::Conflict,
                message,
                code: Some(crate::app_error_codes::SOURCE_BODY_UNAVAILABLE),
            };
        }
        Self {
            kind,
            message,
            code: None,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for AppError {}

/// Failure of a persistence tail whose room state may already be committed.
/// Keep permanent refusals typed until the executor decides whether to retain
/// the operation. Legacy string-only storage failures remain retryable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PersistenceError {
    Permanent(String),
    Transient(String),
}

impl PersistenceError {
    pub(crate) fn permanent(message: impl Into<String>) -> Self {
        Self::Permanent(message.into())
    }

    pub(crate) fn with_context(self, context: impl fmt::Display) -> Self {
        match self {
            Self::Permanent(message) => Self::Permanent(format!("{context}: {message}")),
            Self::Transient(message) => Self::Transient(format!("{context}: {message}")),
        }
    }

    pub(crate) fn io(error: std::io::Error, context: impl fmt::Display) -> Self {
        let message = format!("{context}: {error}");
        match error.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
                Self::Permanent(message)
            }
            _ => Self::Transient(message),
        }
    }
}

impl From<String> for PersistenceError {
    fn from(message: String) -> Self {
        Self::Transient(message)
    }
}

impl From<&str> for PersistenceError {
    fn from(message: &str) -> Self {
        Self::Transient(message.to_string())
    }
}

impl From<AppError> for PersistenceError {
    fn from(error: AppError) -> Self {
        if error.code() == Some("permanent_persistence_failure") {
            return Self::Permanent(error.message());
        }
        match error.kind() {
            AppErrorKind::Validation | AppErrorKind::NotFound | AppErrorKind::Conflict => {
                Self::Permanent(error.message())
            }
            _ => Self::Transient(error.message()),
        }
    }
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Permanent(message) | Self::Transient(message) => formatter.write_str(message),
        }
    }
}

impl Error for PersistenceError {}

impl std::ops::Deref for PersistenceError {
    type Target = str;

    fn deref(&self) -> &str {
        match self {
            Self::Permanent(message) | Self::Transient(message) => message,
        }
    }
}

impl From<PersistenceError> for String {
    fn from(error: PersistenceError) -> Self {
        match error {
            PersistenceError::Permanent(message) | PersistenceError::Transient(message) => message,
        }
    }
}

impl From<PersistenceError> for AppError {
    fn from(error: PersistenceError) -> Self {
        match error {
            PersistenceError::Permanent(message) => {
                Self::storage(message).with_code("permanent_persistence_failure")
            }
            PersistenceError::Transient(message) => Self::storage(message),
        }
    }
}

impl From<AppError> for String {
    fn from(error: AppError) -> Self {
        error.message
    }
}
