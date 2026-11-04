//! The event log. Every transition, agent step, artifact write and error is an `Event`
//! appended to `events.jsonl` and broadcast in-process for the dashboard.

use crate::error::{StageError, StopReason};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// The fixed pipeline, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageId {
    Intake,
    Context,
    Repro,
    Rca,
    Fix,
    Verify,
    Pr,
}

impl StageId {
    pub const ALL: [StageId; 7] =
        [StageId::Intake, StageId::Context, StageId::Repro, StageId::Rca, StageId::Fix, StageId::Verify, StageId::Pr];

    pub fn next(self) -> Option<StageId> {
        let i = Self::ALL.iter().position(|s| *s == self)?;
        Self::ALL.get(i + 1).copied()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            StageId::Intake => "intake",
            StageId::Context => "context",
            StageId::Repro => "repro",
            StageId::Rca => "rca",
            StageId::Fix => "fix",
            StageId::Verify => "verify",
            StageId::Pr => "pr",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StageId::Intake => "S0 Intake",
            StageId::Context => "S1 Context",
            StageId::Repro => "S2 Repro",
            StageId::Rca => "S3 RCA",
            StageId::Fix => "S4 Fix",
            StageId::Verify => "S5 Verify",
            StageId::Pr => "S6 Draft PR",
        }
    }

    pub fn parse(s: &str) -> Option<StageId> {
        Self::ALL.iter().copied().find(|st| st.as_str() == s)
    }

    /// Artifact sub-directory for this stage (e.g. `20-repro`).
    pub fn artifact_dir(self) -> &'static str {
        match self {
            StageId::Intake => "",
            StageId::Context => "10-context",
            StageId::Repro => "20-repro",
            StageId::Rca => "30-rca",
            StageId::Fix => "40-fix",
            StageId::Verify => "50-verify",
            StageId::Pr => "60-pr",
        }
    }
}

impl fmt::Display for StageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a runtime (Claude Code / Codex) reports while it works, normalised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Started { session: Option<String> },
    Text { text: String },
    Thinking { text: String },
    ToolCall { name: String, summary: String },
    ToolResult { ok: bool, summary: String },
    FileChanged { path: String },
    Finished { ok: bool, summary: String },
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    RunCreated { request: crate::state::RunRequest },
    StageStarted { stage: StageId, attempt: u32 },
    StageCompleted { stage: StageId, attempt: u32, duration_ms: u64 },
    StageFailed { stage: StageId, attempt: u32, error: StageError },
    RunStopped { stage: StageId, reason: StopReason, message: String },
    RunFinished { pr_url: Option<String> },
    RunCancelled { stage: Option<StageId> },
    RunInterrupted { stage: Option<StageId> },
    RetryRequested { from: StageId },
    Agent { stage: StageId, provider: String, event: AgentEvent },
    ArtifactWritten { path: String },
    Log { level: Level, message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Monotonic per run; also the SSE id.
    pub seq: u64,
    /// RFC 3339 timestamp.
    pub ts: String,
    pub run_id: String,
    #[serde(flatten)]
    pub kind: EventKind,
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Appends events to `events.jsonl` and publishes them on a broadcast channel.
/// Cheap to clone; all clones share the file and the sequence counter.
#[derive(Clone)]
pub struct EventSink {
    inner: Arc<Mutex<SinkInner>>,
    tx: broadcast::Sender<Event>,
    run_id: String,
}

struct SinkInner {
    file: Option<File>,
    seq: u64,
}

impl EventSink {
    /// Opens (or creates) `<path>` for appending. The sequence continues from the last line.
    pub fn open(run_id: impl Into<String>, path: &Path) -> std::io::Result<Self> {
        let run_id = run_id.into();
        let seq = if path.exists() { read_events(path)?.last().map(|e| e.seq).unwrap_or(0) } else { 0 };
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let (tx, _rx) = broadcast::channel(4096);
        Ok(Self { inner: Arc::new(Mutex::new(SinkInner { file: Some(file), seq })), tx, run_id })
    }

    /// An in-memory sink (tests, dry runs).
    pub fn memory(run_id: impl Into<String>) -> Self {
        let (tx, _rx) = broadcast::channel(4096);
        Self { inner: Arc::new(Mutex::new(SinkInner { file: None, seq: 0 })), tx, run_id: run_id.into() }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn emit(&self, kind: EventKind) -> Event {
        let mut inner = self.inner.lock().expect("event sink poisoned");
        inner.seq += 1;
        let ev = Event { seq: inner.seq, ts: now_rfc3339(), run_id: self.run_id.clone(), kind };
        if let Some(f) = inner.file.as_mut() {
            if let Ok(line) = serde_json::to_string(&ev) {
                let _ = f.write_all(line.as_bytes());
                let _ = f.write_all(b"\n");
                let _ = f.flush();
            }
        }
        drop(inner);
        let _ = self.tx.send(ev.clone());
        ev
    }

    pub fn log(&self, level: Level, message: impl Into<String>) {
        self.emit(EventKind::Log { level, message: message.into() });
    }
    pub fn info(&self, message: impl Into<String>) {
        self.log(Level::Info, message)
    }
    pub fn warn(&self, message: impl Into<String>) {
        self.log(Level::Warn, message)
    }
    pub fn agent(&self, stage: StageId, provider: &str, event: AgentEvent) {
        self.emit(EventKind::Agent { stage, provider: provider.to_string(), event });
    }
    pub fn artifact(&self, rel_path: impl Into<String>) {
        self.emit(EventKind::ArtifactWritten { path: rel_path.into() });
    }
}

/// Reads all events from an `events.jsonl` file; tolerates blank or malformed lines.
pub fn read_events(path: &Path) -> std::io::Result<Vec<Event>> {
    let f = File::open(path)?;
    let mut out = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_str::<Event>(&line) {
            out.push(ev);
        }
    }
    Ok(out)
}

pub fn events_path(run_dir: &Path) -> PathBuf {
    run_dir.join("events.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_order_and_parse() {
        assert_eq!(StageId::Intake.next(), Some(StageId::Context));
        assert_eq!(StageId::Pr.next(), None);
        assert_eq!(StageId::parse("verify"), Some(StageId::Verify));
        assert_eq!(StageId::parse("nope"), None);
    }

    #[test]
    fn sink_writes_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let sink = EventSink::open("r1", &path).unwrap();
        let mut rx = sink.subscribe();
        sink.info("hello");
        sink.emit(EventKind::StageStarted { stage: StageId::Intake, attempt: 1 });
        let got = rx.try_recv().unwrap();
        assert_eq!(got.seq, 1);
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].seq, 2);
        // Reopening continues the sequence.
        let sink2 = EventSink::open("r1", &path).unwrap();
        let ev = sink2.emit(EventKind::Log { level: Level::Info, message: "again".into() });
        assert_eq!(ev.seq, 3);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, "not json\n\n{\"seq\":1,\"ts\":\"t\",\"run_id\":\"r\",\"kind\":\"log\",\"level\":\"info\",\"message\":\"m\"}\n").unwrap();
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 1);
    }
}
