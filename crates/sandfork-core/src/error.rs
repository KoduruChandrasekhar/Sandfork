//! Error classes. The class is part of the API contract (Part 7).

use serde::{Deserialize, Serialize};

/// Who is at fault. The orchestrator branches on this and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorClass {
    /// Belongs to the trajectory: a command failed, timed out or ran out of memory.
    Task,
    /// The rollout is retried or dropped: a sandbox died, the spawner failed, CRIU failed.
    Infra,
    /// A bad request.
    Client,
}

impl ErrorClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorClass::Task => "TASK",
            ErrorClass::Infra => "INFRA",
            ErrorClass::Client => "CLIENT",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{}/{code}: {message}", class.as_str())]
pub struct SfError {
    pub class: ErrorClass,
    pub code: String,
    pub message: String,
}

impl SfError {
    pub fn new(class: ErrorClass, code: &str, message: impl Into<String>) -> Self {
        SfError {
            class,
            code: code.to_string(),
            message: message.into(),
        }
    }
    pub fn infra(code: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorClass::Infra, code, message)
    }
    pub fn task(code: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorClass::Task, code, message)
    }
    pub fn client(code: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorClass::Client, code, message)
    }
    /// Build an infrastructure error from the current `errno`.
    pub fn errno(what: &str) -> Self {
        let e = std::io::Error::last_os_error();
        Self::infra("syscall", format!("{what}: {e}"))
    }
}

impl From<std::io::Error> for SfError {
    fn from(e: std::io::Error) -> Self {
        SfError::infra("io", e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SfError>;

/// Attach "what was being attempted" to an error, keeping it an infra error.
pub trait Ctx<T> {
    fn ctx(self, what: &str) -> Result<T>;
    fn ctx_with<F: FnOnce() -> String>(self, f: F) -> Result<T>;
}

impl<T, E: std::fmt::Display> Ctx<T> for std::result::Result<T, E> {
    fn ctx(self, what: &str) -> Result<T> {
        self.map_err(|e| SfError::infra("io", format!("{what}: {e}")))
    }
    fn ctx_with<F: FnOnce() -> String>(self, f: F) -> Result<T> {
        self.map_err(|e| SfError::infra("io", format!("{}: {e}", f())))
    }
}
