//! The `Agent` trait: one call into a coding-agent CLI (Claude Code or Codex) with a prompt,
//! a mode (read-only or edit), and an optional JSON schema for the structured answer.

pub mod claude;
pub mod codex;

use crate::config::{ProviderConfig, ProviderKind};
use crate::events::{EventSink, StageId};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// File reading and search only; no commands, no edits (S1, S3).
    ReadOnly,
    /// May edit files and run cargo/git in the worktree (S2, S4).
    Edit,
}

#[derive(Debug, Clone)]
pub struct AgentTask {
    pub stage: StageId,
    /// The worktree; the agent's working directory.
    pub cwd: PathBuf,
    /// `~/.lyra/runs/<id>/artifacts`, made readable to the agent (`--add-dir`).
    pub artifacts: PathBuf,
    pub prompt: String,
    pub mode: Mode,
    /// JSON Schema the final answer must match (delivered through the runtime's own flag).
    pub schema: Option<Value>,
    /// Where the raw runtime output (JSONL) is appended.
    pub transcript: PathBuf,
    pub timeout: Duration,
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone, Default)]
pub struct AgentOutput {
    /// Final free-text message from the agent (may be empty when structured output is used).
    pub text: String,
    /// Validated-by-runtime structured answer, if a schema was given.
    pub structured: Option<Value>,
    /// True when the runtime reported success and (if a schema was given) produced structured output.
    pub ok: bool,
    /// Runtime-reported failure reason when `ok` is false (turn.failed, permission denials, ...).
    pub failure: Option<String>,
    pub transcript: PathBuf,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
}

#[async_trait]
pub trait Agent: Send + Sync {
    /// Provider id as configured (`claude`, `codex`).
    fn id(&self) -> &str;
    /// Human-readable runtime + model for disclosure lines.
    fn describe(&self) -> String;
    /// Runs one agent call, forwarding normalised events to `events` under `task.stage`.
    async fn run(&self, task: AgentTask, events: &EventSink) -> Result<AgentOutput>;
}

pub type SharedAgent = Arc<dyn Agent>;

/// Builds the adapter for a configured provider.
pub fn make_agent(id: &str, cfg: &ProviderConfig) -> SharedAgent {
    match cfg.kind {
        ProviderKind::Claude => Arc::new(claude::ClaudeAgent::new(id, cfg)),
        ProviderKind::Codex => Arc::new(codex::CodexAgent::new(id, cfg)),
    }
}

/// Appends a raw line to the transcript file (best effort).
pub(crate) fn append_transcript(path: &PathBuf, line: &str) {
    use std::io::Write;
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
        let _ = f.write_all(b"\n");
    }
}
