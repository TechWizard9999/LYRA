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
    /// Codex has no native file-reader tool, so it receives a harness source snapshot.
    ReadOnly,
    /// May edit files in the worktree (S2, S4); the harness executes commands.
    Edit,
}

#[derive(Debug, Clone)]
pub struct AgentTask {
    pub stage: StageId,
    /// The worktree; the agent's working directory.
    pub cwd: PathBuf,
    /// Harness-owned output directory. Never granted as an agent workspace.
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
    /// Structured answer validated by both the runtime and the harness.
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

/// Runtime schema flags are an aid, not a trust boundary. In particular a JSON-looking
/// free-text fallback must not bypass required fields or enum/type constraints.
pub(crate) fn validate_output(schema: Option<&Value>, output: Option<&Value>) -> Result<()> {
    let Some(schema) = schema else { return Ok(()) };
    let output = output.ok_or_else(|| anyhow::anyhow!("no structured output"))?;
    let validator = jsonschema::validator_for(schema).map_err(|e| anyhow::anyhow!("invalid output schema: {e}"))?;
    if let Some(error) = validator.iter_errors(output).next() {
        anyhow::bail!("structured output failed schema validation at {}: {}", error.instance_path, error);
    }
    Ok(())
}

/// Extra arguments may tune runtime behavior, but must not replace LYRA's tool,
/// workspace, auth, or sandbox policy. Keep this deliberately small and explicit.
pub(crate) fn validate_extra_args(args: &[String], valued: &[&str], flags: &[&str]) -> Result<()> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if flags.contains(&arg.as_str()) {
            i += 1;
        } else if valued.contains(&arg.as_str()) {
            let value = args
                .get(i + 1)
                .filter(|s| !s.starts_with('-'))
                .ok_or_else(|| anyhow::anyhow!("missing value for {arg}"))?;
            anyhow::ensure!(!value.is_empty(), "empty value for {arg}");
            i += 2;
        } else {
            anyhow::bail!("unsupported provider extra argument {arg:?}; it may override LYRA's execution policy");
        }
    }
    Ok(())
}

const MAX_TRANSCRIPT_BYTES: u64 = 8 * 1024 * 1024;
const TRANSCRIPT_TRUNCATED: &[u8] = b"# transcript truncated: remaining provider output omitted\n";

/// Retain bounded raw evidence while continuing to parse the provider stream.
pub(crate) struct Transcript {
    file: std::fs::File,
    written: u64,
    pub truncated: bool,
}

impl Transcript {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata()?.len();
        Ok(Self { file, written, truncated: false })
    }

    pub fn append(&mut self, line: &str) -> Result<()> {
        use std::io::Write;
        if self.truncated {
            return Ok(());
        }
        let limit = MAX_TRANSCRIPT_BYTES - TRANSCRIPT_TRUNCATED.len() as u64;
        if self.written.saturating_add(line.len() as u64).saturating_add(1) > limit {
            // Also bound transcripts left by older versions before resuming appends.
            if self.written > limit {
                self.file.set_len(limit)?;
            }
            self.file.write_all(TRANSCRIPT_TRUNCATED)?;
            self.truncated = true;
        } else {
            self.file.write_all(line.as_bytes())?;
            self.file.write_all(b"\n")?;
            self.written += line.len() as u64 + 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn transcript_retention_is_bounded_and_marks_truncation_once() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("transcript.jsonl");
        let mut transcript = Transcript::open(&path).unwrap();
        transcript.append("first record").unwrap();
        let record = "x".repeat(1024 * 1024);
        for _ in 0..20 {
            transcript.append(&record).unwrap();
        }
        assert!(transcript.truncated);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.len() as u64 <= MAX_TRANSCRIPT_BYTES);
        assert!(contents.starts_with("first record\n"));
        assert_eq!(contents.matches("# transcript truncated:").count(), 1);
    }

    #[test]
    fn transcript_creation_errors_are_returned() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), "not a directory").unwrap();
        assert!(Transcript::open(&root.path().join("file/transcript.jsonl")).is_err());
    }

    #[test]
    fn schema_validation_rejects_empty_wrong_types_and_unknown_enum() {
        let schema = json!({"type":"object", "required":["result"], "properties":{"result":{"type":"string","enum":["supported","unresolved"]}}});
        for value in [json!({}), json!({"result":false}), json!({"result":"looks fine"})] {
            assert!(validate_output(Some(&schema), Some(&value)).is_err());
        }
        assert!(validate_output(Some(&schema), Some(&json!({"result":"supported"}))).is_ok());
        assert!(validate_output(Some(&schema), None).is_err());
    }

    #[test]
    fn extra_arguments_cannot_override_execution_policy() {
        for flag in ["--tools", "--add-dir", "--settings", "--dangerously-skip-permissions", "-c"] {
            assert!(validate_extra_args(&[flag.into(), "x".into()], &["--effort"], &[]).is_err());
        }
        assert!(validate_extra_args(&["--effort".into(), "high".into()], &["--effort"], &[]).is_ok());
        assert!(validate_extra_args(&["--max-budget-usd".into(), "1".into()], &["--effort"], &[]).is_err());
    }
}
