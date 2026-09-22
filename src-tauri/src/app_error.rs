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
            return Self { kind: AppErrorKind::Conflict, message,
                code: Some(crate::app_error_codes::SOURCE_BODY_UNAVAILABLE) };
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

impl From<AppError> for String {
    fn from(error: AppError) -> Self {
        error.message
    }
}
