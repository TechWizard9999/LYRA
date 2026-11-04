//! Typed stage errors and outcomes. Every stage returns a `StageOutcome`; the engine
//! decides what to do with it (advance, stop the run, or mark it failed and wait for a retry).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Provider,
    Tool,
    Policy,
    Git,
    GitHub,
    Io,
    Schema,
    Guard,
    Timeout,
    Config,
}

/// A failure of one stage attempt. `retryable` tells the UI whether "Retry from stage"
/// is likely to help; the engine never retries automatically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageError {
    pub kind: ErrorKind,
    pub message: String,
    pub retryable: bool,
    /// Path (inside the run dir) to a log or transcript that explains the failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<PathBuf>,
}

impl StageError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), retryable: true, details: None }
    }
    pub fn fatal(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), retryable: false, details: None }
    }
    pub fn with_details(mut self, path: impl Into<PathBuf>) -> Self {
        self.details = Some(path.into());
        self
    }
    pub fn from_anyhow(kind: ErrorKind, err: anyhow::Error) -> Self {
        Self::new(kind, format!("{err:#}"))
    }
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for StageError {}

/// Why a run stopped deliberately (not an error: the harness did its job and found a wall).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    NotReproduced,
    RcaUnresolved,
    VerifyFailed,
    PolicyNotAcknowledged,
    NoPushAccess,
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            StopReason::NotReproduced => "the issue could not be reproduced",
            StopReason::RcaUnresolved => "root cause analysis was unresolved",
            StopReason::VerifyFailed => "verification failed",
            StopReason::PolicyNotAcknowledged => "repository policy not acknowledged",
            StopReason::NoPushAccess => "no push access to the repository",
        };
        f.write_str(s)
    }
}

/// Result of running one stage attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum StageOutcome {
    Completed,
    Stopped { reason: StopReason, message: String },
    Failed { error: StageError },
}

impl StageOutcome {
    pub fn failed(error: StageError) -> Self {
        StageOutcome::Failed { error }
    }
    pub fn stopped(reason: StopReason, message: impl Into<String>) -> Self {
        StageOutcome::Stopped { reason, message: message.into() }
    }
}

impl From<StageError> for StageOutcome {
    fn from(error: StageError) -> Self {
        StageOutcome::Failed { error }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_error_roundtrips_through_json() {
        let e = StageError::new(ErrorKind::Guard, "test file changed").with_details("50-verify/x.txt");
        let s = serde_json::to_string(&e).unwrap();
        let back: StageError = serde_json::from_str(&s).unwrap();
        assert_eq!(back.kind, ErrorKind::Guard);
        assert!(back.retryable);
        assert_eq!(back.details.unwrap(), PathBuf::from("50-verify/x.txt"));
    }

    #[test]
    fn outcome_tagging() {
        let o = StageOutcome::stopped(StopReason::NotReproduced, "no");
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["outcome"], "stopped");
        assert_eq!(v["reason"], "not_reproduced");
    }
}
