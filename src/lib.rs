//! LYRA: a harness around coding-agent CLIs that turns a GitHub issue into a draft PR.
//!
//! Module map (see docs/PLAN.md):
//! - `config`    : `~/.lyra/config.toml` + `<repo>/lyra.toml`
//! - `events`    : the append-only event log and live broadcast
//! - `state`     : `run.json` (status, stage, checkpoints, attempts)
//! - `engine`    : fixed pipeline driver, retry-from-stage, cancel, hang guard
//! - `stages`    : S0..S6 implementations
//! - `guards`    : universal repro / fix / verify rules
//! - `profile`   : execution profiles: the target repository's toolchain and test runner
//! - `providers` : the `Agent` trait and the Claude Code / Codex adapters
//! - `proc`, `git`, `github`, `artifacts`, `doctor`: plumbing

pub mod artifacts;
pub mod config;
pub mod doctor;
pub mod engine;
pub mod error;
pub mod events;
pub mod git;
pub mod github;
pub mod guards;
pub mod proc;
pub mod profile;
pub mod providers;
pub mod stages;
pub mod state;
pub mod web;

pub use error::{ErrorKind, StageError, StageOutcome, StopReason};
pub use events::{AgentEvent, Event, EventSink, StageId};
pub use state::{RunId, RunRequest, RunState, Status};
