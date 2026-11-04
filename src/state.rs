//! `run.json`: the checkpointed state of one run. Rewritten on every transition.

use crate::error::{ErrorKind, StageError, StopReason};
use crate::events::StageId;
use crate::guards::{self, ReproRecord};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub type RunId = String;

/// What the user submitted (form, CLI or API).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRequest {
    /// Local path of an already-cloned repository.
    pub repo: PathBuf,
    /// Issue URL, `#123`, or `123`. If `issue_text` is set this is only a label.
    pub issue: String,
    /// Pasted issue text for offline runs (skips `gh issue view`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_text: Option<String>,
    /// Provider id from config (`claude` | `codex`). None = config default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The user ticked "I read the repository's contribution policy".
    #[serde(default)]
    pub policy_acknowledged: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Running,
    Stopped,
    Failed,
    Done,
    Interrupted,
    Cancelled,
}

impl Status {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Status::Queued | Status::Running)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Running => "running",
            Status::Stopped => "stopped",
            Status::Failed => "failed",
            Status::Done => "done",
            Status::Interrupted => "interrupted",
            Status::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Pending,
    Running,
    Done,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub stage: StageId,
    pub n: u32,
    /// HEAD of the worktree when the attempt started.
    pub input_commit: Option<String>,
    pub started: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<String>,
    pub result: AttemptResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttemptResult {
    Running,
    Completed { duration_ms: u64 },
    Failed { error: StageError },
    Stopped { reason: StopReason, message: String },
    Cancelled,
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunState {
    pub id: RunId,
    pub request: RunRequest,
    pub provider: String,
    pub status: Status,
    /// Stage currently running, or the one to retry from.
    pub stage: StageId,
    pub stages: BTreeMap<StageId, StageStatus>,
    pub created: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<String>,
    // Repository facts filled by intake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_slug: Option<String>,
    /// Repository owning the issue (may differ from a local fork's origin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_slug: Option<String>,
    /// Commits recorded after S2 (test) and S4 (fix).
    #[serde(default)]
    pub checkpoints: BTreeMap<StageId, String>,
    /// Harness-owned S2 evidence. Artifact files are display copies, never authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_repro: Option<ReproRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<StageError>,
    #[serde(default)]
    pub attempts: Vec<Attempt>,
    /// Whether intake found a contribution/AI policy file.
    #[serde(default)]
    pub has_policy: bool,
}

pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

impl RunState {
    pub fn new(id: RunId, request: RunRequest, provider: String) -> Self {
        let stages = StageId::ALL.iter().map(|s| (*s, StageStatus::Pending)).collect();
        Self {
            id,
            request,
            provider,
            status: Status::Queued,
            stage: StageId::Intake,
            stages,
            created: now_rfc3339(),
            finished: None,
            base_commit: None,
            branch: None,
            issue_number: None,
            repo_slug: None,
            issue_slug: None,
            checkpoints: BTreeMap::new(),
            frozen_repro: None,
            verified_commit: None,
            pr_url: None,
            stop_reason: None,
            last_error: None,
            attempts: Vec::new(),
            has_policy: false,
        }
    }

    pub fn path(run_dir: &Path) -> PathBuf {
        run_dir.join("run.json")
    }

    pub fn load(run_dir: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(Self::path(run_dir))?;
        Ok(serde_json::from_str(&s)?)
    }

    /// Atomic rewrite (write temp, rename).
    pub fn save(&self, run_dir: &Path) -> anyhow::Result<()> {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = Self::path(run_dir);
        let tmp =
            run_dir.join(format!("run.json.{}.{}.tmp", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed)));
        let result = (|| -> anyhow::Result<()> {
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            file.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    pub fn require_frozen_repro(&self) -> Result<&ReproRecord, StageError> {
        let retry = |reason: &str| StageError::new(ErrorKind::Guard, format!("{reason}; retry from repro"));
        let rec = self.frozen_repro.as_ref().ok_or_else(|| retry("frozen reproduction record is missing"))?;
        guards::validate_repro_record(rec).map_err(|reason| retry(&reason))?;
        let checkpoint = self.checkpoints.get(&StageId::Repro).ok_or_else(|| retry("repro checkpoint is missing"))?;
        if rec.frozen_commit.as_ref() != Some(checkpoint) {
            return Err(retry("frozen reproduction record does not match the S2 checkpoint"));
        }
        Ok(rec)
    }

    pub fn stage_status(&self, stage: StageId) -> StageStatus {
        self.stages.get(&stage).copied().unwrap_or(StageStatus::Pending)
    }

    pub fn set_stage_status(&mut self, stage: StageId, status: StageStatus) {
        self.stages.insert(stage, status);
    }

    pub fn attempt_count(&self, stage: StageId) -> u32 {
        self.attempts.iter().filter(|a| a.stage == stage).count() as u32
    }

    /// Resets state for stages >= `from` so they can run again. Checkpoints and verified
    /// commit that belong to invalidated stages are cleared; attempts are marked superseded.
    pub fn invalidate_from(&mut self, from: StageId) {
        for s in StageId::ALL.iter().copied().filter(|s| *s >= from) {
            self.stages.insert(s, StageStatus::Pending);
            self.checkpoints.remove(&s);
        }
        if from <= StageId::Verify {
            self.verified_commit = None;
        }
        if from <= StageId::Repro {
            self.frozen_repro = None;
        }
        if from <= StageId::Pr {
            self.pr_url = None;
        }
        for a in self.attempts.iter_mut().filter(|a| a.stage >= from) {
            if !matches!(a.result, AttemptResult::Superseded) {
                a.result = AttemptResult::Superseded;
            }
        }
        self.stop_reason = None;
        self.last_error = None;
        self.finished = None;
        self.stage = from;
        self.status = Status::Queued;
    }

    /// The commit to reset the worktree to before running `stage` again.
    pub fn checkpoint_before(&self, stage: StageId) -> Option<&String> {
        let mut best: Option<&String> = None;
        for s in StageId::ALL.iter().copied().filter(|s| *s < stage) {
            if let Some(c) = self.checkpoints.get(&s) {
                best = Some(c);
            }
        }
        best.or(self.base_commit.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    fn req() -> RunRequest {
        RunRequest {
            repo: PathBuf::from("/tmp/repo"),
            issue: "#1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = RunState::new("r1".into(), req(), "claude".into());
        st.checkpoints.insert(StageId::Repro, "abc".into());
        st.save(dir.path()).unwrap();
        let back = RunState::load(dir.path()).unwrap();
        assert_eq!(back, st);
        st.has_policy = true;
        st.save(dir.path()).unwrap();
        assert_eq!(RunState::load(dir.path()).unwrap(), st);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    fn frozen_record() -> ReproRecord {
        serde_json::from_value(serde_json::json!({
            "reproduced": true, "crate": "fixture", "test_file": "tests/issue_1.rs",
            "test_name": "issue_1_bug", "expected": "the bug", "frozen_commit": "s2"
        }))
        .unwrap()
    }

    #[test]
    fn legacy_state_loads_but_cannot_certify_without_frozen_reproduction() {
        let state = RunState::new("legacy".into(), req(), "claude".into());
        let mut value = serde_json::to_value(&state).unwrap();
        value.as_object_mut().unwrap().remove("frozen_repro");
        value.as_object_mut().unwrap().remove("issue_slug");
        let loaded: RunState = serde_json::from_value(value).unwrap();
        let error = loaded.require_frozen_repro().unwrap_err();
        assert!(error.message.contains("retry from repro"));
    }

    #[test]
    fn frozen_record_requires_matching_checkpoint_and_survives_only_later_retries() {
        let mut state = RunState::new("r".into(), req(), "claude".into());
        state.frozen_repro = Some(frozen_record());
        state.checkpoints.insert(StageId::Repro, "different".into());
        assert!(state.require_frozen_repro().is_err());
        state.checkpoints.insert(StageId::Repro, "s2".into());
        assert!(state.require_frozen_repro().is_ok());
        state.invalidate_from(StageId::Fix);
        assert!(state.require_frozen_repro().is_ok());
        state.invalidate_from(StageId::Repro);
        assert!(state.frozen_repro.is_none());
    }

    #[test]
    fn invalidate_from_clears_downstream() {
        let mut st = RunState::new("r1".into(), req(), "claude".into());
        st.base_commit = Some("base".into());
        st.checkpoints.insert(StageId::Repro, "t1".into());
        st.checkpoints.insert(StageId::Fix, "f1".into());
        st.verified_commit = Some("f1".into());
        st.pr_url = Some("url".into());
        st.last_error = Some(StageError::new(ErrorKind::Guard, "x"));
        for s in StageId::ALL {
            st.set_stage_status(s, StageStatus::Done);
        }
        st.attempts.push(Attempt {
            stage: StageId::Fix,
            n: 1,
            input_commit: None,
            started: "t".into(),
            finished: None,
            result: AttemptResult::Completed { duration_ms: 1 },
        });
        st.invalidate_from(StageId::Fix);
        assert_eq!(st.stage, StageId::Fix);
        assert_eq!(st.status, Status::Queued);
        assert_eq!(st.stage_status(StageId::Rca), StageStatus::Done);
        assert_eq!(st.stage_status(StageId::Fix), StageStatus::Pending);
        assert_eq!(st.stage_status(StageId::Pr), StageStatus::Pending);
        assert!(st.checkpoints.contains_key(&StageId::Repro));
        assert!(!st.checkpoints.contains_key(&StageId::Fix));
        assert!(st.verified_commit.is_none());
        assert!(st.pr_url.is_none());
        assert!(st.last_error.is_none());
        assert!(matches!(st.attempts[0].result, AttemptResult::Superseded));
        assert_eq!(st.checkpoint_before(StageId::Fix).unwrap(), "t1");
        assert_eq!(st.checkpoint_before(StageId::Repro).unwrap(), "base");
    }
}
