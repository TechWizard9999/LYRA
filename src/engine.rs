//! The engine: creates runs, drives the fixed pipeline one stage at a time, persists
//! `run.json` and `events.jsonl` on every transition, and implements cancel and
//! retry-from-stage (archive outputs, restore the git checkpoint, invalidate later stages).
//! One active run at a time.

use crate::artifacts::RunDir;
use crate::config::{runs_dir, Config};
use crate::error::StageOutcome;
use crate::events::{read_events, Event, EventKind, EventSink, StageId};
use crate::git;
use crate::providers::{make_agent, SharedAgent};
use crate::stages::{pipeline, StageCtx};
use crate::state::{now_rfc3339, Attempt, AttemptResult, RunId, RunRequest, RunState, StageStatus, Status};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct Active {
    cancel: CancellationToken,
    sink: EventSink,
}

pub struct Engine {
    home: PathBuf,
    config: Config,
    active: Mutex<HashMap<RunId, Active>>,
    /// Sinks for finished runs stay around so late subscribers can still attach.
    sinks: Mutex<HashMap<RunId, EventSink>>,
}

impl Engine {
    pub fn new(home: PathBuf, config: Config) -> Arc<Self> {
        Arc::new(Self { home, config, active: Mutex::new(HashMap::new()), sinks: Mutex::new(HashMap::new()) })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn run_dir(&self, id: &str) -> RunDir {
        RunDir::new(runs_dir(&self.home).join(id))
    }

    pub fn is_busy(&self) -> bool {
        !self.active.lock().unwrap().is_empty()
    }

    /// Generates `<owner>-<repo>-<issue>-<timestamp>`-style ids (slug and issue may be unknown yet).
    pub fn new_run_id(req: &RunRequest) -> RunId {
        let repo_name = req.repo.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "repo".into());
        let issue = req.issue.rsplit(['/', '#']).next().filter(|s| !s.is_empty()).unwrap_or("issue").to_string();
        let ts = time::OffsetDateTime::now_utc()
            .format(&time::macros::format_description!("[year][month][day]-[hour][minute][second]"))
            .unwrap_or_else(|_| "0".into());
        let clean = |s: &str| {
            s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect::<String>()
        };
        format!("{}-{}-{}", clean(&repo_name), clean(&issue), ts)
    }

    /// Creates the run directory and state, then starts the pipeline in the background.
    pub fn submit(self: &Arc<Self>, req: RunRequest) -> Result<RunId> {
        if self.is_busy() {
            bail!("a run is already active; wait for it to finish or cancel it");
        }
        let provider = req.provider.clone().unwrap_or_else(|| self.config.run.provider.clone());
        self.config.provider(&provider)?;
        let id = Self::new_run_id(&req);
        let rd = self.run_dir(&id);
        rd.create()?;
        let state = RunState::new(id.clone(), req, provider);
        state.save(&rd.root)?;
        let sink = EventSink::open(&id, &crate::events::events_path(&rd.root))?;
        sink.emit(EventKind::RunCreated { request: state.request.clone() });
        self.spawn(id.clone(), state, sink);
        Ok(id)
    }

    /// Retry from `from`: archive outputs of stages >= from, restore the checkpoint, invalidate, run.
    pub fn retry(self: &Arc<Self>, id: &str, from: StageId) -> Result<()> {
        if self.active.lock().unwrap().contains_key(id) {
            bail!("run {id} is still active");
        }
        if self.is_busy() {
            bail!("another run is active");
        }
        let rd = self.run_dir(id);
        let mut state = RunState::load(&rd.root).with_context(|| format!("loading run {id}"))?;
        if !state.status.is_terminal() {
            bail!("run {id} is not in a terminal state");
        }
        if from == StageId::Intake {
            // Intake recreates the worktree; nothing to restore.
        } else if let Some(cp) = state.checkpoint_before(from).cloned() {
            let wt = rd.worktree();
            if wt.exists() {
                git::reset_hard_clean(&wt, &cp).with_context(|| format!("restoring checkpoint {}", git::short(&cp)))?;
            }
        }
        let n = state.attempts.len() as u32 + 1;
        rd.archive_from(from, n)?;
        state.invalidate_from(from);
        state.save(&rd.root)?;
        let sink = EventSink::open(id, &crate::events::events_path(&rd.root))?;
        sink.emit(EventKind::RetryRequested { from });
        self.spawn(id.to_string(), state, sink);
        Ok(())
    }

    pub fn cancel(&self, id: &str) -> bool {
        if let Some(a) = self.active.lock().unwrap().get(id) {
            a.cancel.cancel();
            true
        } else {
            false
        }
    }

    pub fn get(&self, id: &str) -> Result<RunState> {
        RunState::load(&self.run_dir(id).root)
    }

    /// All runs, newest first.
    pub fn list(&self) -> Vec<RunState> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(runs_dir(&self.home)) {
            for e in rd.flatten() {
                if let Ok(st) = RunState::load(&e.path()) {
                    out.push(st);
                }
            }
        }
        out.sort_by(|a, b| b.created.cmp(&a.created));
        out
    }

    /// Live subscription (None if the run does not exist).
    pub fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<Event>> {
        if let Some(a) = self.active.lock().unwrap().get(id) {
            return Some(a.sink.subscribe());
        }
        if let Some(s) = self.sinks.lock().unwrap().get(id) {
            return Some(s.subscribe());
        }
        if self.run_dir(id).root.exists() {
            let sink = EventSink::open(id, &crate::events::events_path(&self.run_dir(id).root)).ok()?;
            let rx = sink.subscribe();
            self.sinks.lock().unwrap().insert(id.to_string(), sink);
            return Some(rx);
        }
        None
    }

    /// Events so far (replay), optionally after `since` seq.
    pub fn history(&self, id: &str, since: u64) -> Vec<Event> {
        read_events(&crate::events::events_path(&self.run_dir(id).root))
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.seq > since)
            .collect()
    }

    /// On startup: any run left `running`/`queued` by a previous process is marked interrupted.
    pub fn mark_interrupted(&self) -> usize {
        let mut n = 0;
        for mut st in self.list() {
            if !st.status.is_terminal() {
                st.status = Status::Interrupted;
                st.finished = Some(now_rfc3339());
                if let Some(a) = st.attempts.iter_mut().rev().find(|a| matches!(a.result, AttemptResult::Running)) {
                    a.result = AttemptResult::Cancelled;
                    a.finished = Some(now_rfc3339());
                }
                let rd = self.run_dir(&st.id);
                let _ = st.save(&rd.root);
                if let Ok(sink) = EventSink::open(&st.id, &crate::events::events_path(&rd.root)) {
                    sink.emit(EventKind::RunInterrupted { stage: Some(st.stage) });
                }
                n += 1;
            }
        }
        n
    }

    fn spawn(self: &Arc<Self>, id: RunId, state: RunState, sink: EventSink) {
        let cancel = CancellationToken::new();
        self.active.lock().unwrap().insert(id.clone(), Active { cancel: cancel.clone(), sink: sink.clone() });
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            let result = engine.drive(state, sink.clone(), cancel).await;
            if let Err(e) = result {
                sink.warn(format!("engine error: {e:#}"));
            }
            engine.active.lock().unwrap().remove(&id);
            engine.sinks.lock().unwrap().insert(id, sink);
        });
    }

    async fn drive(self: &Arc<Self>, mut state: RunState, sink: EventSink, cancel: CancellationToken) -> Result<()> {
        let rd = self.run_dir(&state.id);
        let config =
            self.config.clone().with_repo_overrides(&state.request.repo).unwrap_or_else(|_| self.config.clone());
        let pcfg = config.provider(&state.provider)?.clone();
        let agent: SharedAgent = make_agent(&state.provider, &pcfg);

        state.status = Status::Running;
        state.save(&rd.root)?;

        let stages = pipeline();
        let start_idx = stages.iter().position(|s| s.id() == state.stage).unwrap_or(0);
        let mut ctx =
            StageCtx { run_dir: rd.clone(), state, config, agent, events: sink.clone(), cancel: cancel.clone() };

        for stage in stages.iter().skip(start_idx) {
            let id = stage.id();
            if cancel.is_cancelled() {
                break;
            }
            let attempt_n = ctx.state.attempt_count(id) + 1;
            let input_commit = git::head(&rd.worktree()).ok();
            ctx.state.stage = id;
            ctx.state.set_stage_status(id, StageStatus::Running);
            ctx.state.attempts.push(Attempt {
                stage: id,
                n: attempt_n,
                input_commit,
                started: now_rfc3339(),
                finished: None,
                result: AttemptResult::Running,
            });
            ctx.state.save(&rd.root)?;
            sink.emit(EventKind::StageStarted { stage: id, attempt: attempt_n });
            let started = std::time::Instant::now();

            let outcome = stage.run(&mut ctx).await;
            let duration_ms = started.elapsed().as_millis() as u64;

            if cancel.is_cancelled() {
                finish_attempt(&mut ctx.state, AttemptResult::Cancelled);
                ctx.state.set_stage_status(id, StageStatus::Failed);
                ctx.state.status = Status::Cancelled;
                ctx.state.finished = Some(now_rfc3339());
                ctx.state.save(&rd.root)?;
                sink.emit(EventKind::RunCancelled { stage: Some(id) });
                return Ok(());
            }

            match outcome {
                StageOutcome::Completed => {
                    finish_attempt(&mut ctx.state, AttemptResult::Completed { duration_ms });
                    ctx.state.set_stage_status(id, StageStatus::Done);
                    ctx.state.save(&rd.root)?;
                    sink.emit(EventKind::StageCompleted { stage: id, attempt: attempt_n, duration_ms });
                }
                StageOutcome::Stopped { reason, message } => {
                    finish_attempt(
                        &mut ctx.state,
                        AttemptResult::Stopped { reason: reason.clone(), message: message.clone() },
                    );
                    ctx.state.set_stage_status(id, StageStatus::Stopped);
                    ctx.state.status = Status::Stopped;
                    ctx.state.stop_reason = Some(reason.clone());
                    ctx.state.finished = Some(now_rfc3339());
                    ctx.state.save(&rd.root)?;
                    sink.emit(EventKind::RunStopped { stage: id, reason, message });
                    return Ok(());
                }
                StageOutcome::Failed { error } => {
                    finish_attempt(&mut ctx.state, AttemptResult::Failed { error: error.clone() });
                    ctx.state.set_stage_status(id, StageStatus::Failed);
                    ctx.state.status = Status::Failed;
                    ctx.state.last_error = Some(error.clone());
                    ctx.state.finished = Some(now_rfc3339());
                    ctx.state.save(&rd.root)?;
                    sink.emit(EventKind::StageFailed { stage: id, attempt: attempt_n, error });
                    return Ok(());
                }
            }
        }

        if cancel.is_cancelled() {
            ctx.state.status = Status::Cancelled;
            ctx.state.finished = Some(now_rfc3339());
            ctx.state.save(&rd.root)?;
            sink.emit(EventKind::RunCancelled { stage: None });
            return Ok(());
        }
        ctx.state.status = Status::Done;
        ctx.state.finished = Some(now_rfc3339());
        ctx.state.save(&rd.root)?;
        sink.emit(EventKind::RunFinished { pr_url: ctx.state.pr_url.clone() });
        Ok(())
    }
}

fn finish_attempt(state: &mut RunState, result: AttemptResult) {
    if let Some(a) = state.attempts.iter_mut().rev().find(|a| matches!(a.result, AttemptResult::Running)) {
        a.result = result;
        a.finished = Some(now_rfc3339());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_is_filesystem_safe() {
        let req = RunRequest {
            repo: PathBuf::from("C:/src/My Crate"),
            issue: "https://github.com/o/r/issues/42".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
        };
        let id = Engine::new_run_id(&req);
        assert!(id.starts_with("my-crate-42-"), "{id}");
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn list_and_mark_interrupted() {
        let home = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().to_path_buf(), Config::default());
        let rd = engine.run_dir("r1");
        rd.create().unwrap();
        let req = RunRequest {
            repo: PathBuf::from("."),
            issue: "#1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
        };
        let mut st = RunState::new("r1".into(), req, "claude".into());
        st.status = Status::Running;
        st.save(&rd.root).unwrap();
        assert_eq!(engine.list().len(), 1);
        assert_eq!(engine.mark_interrupted(), 1);
        assert_eq!(engine.get("r1").unwrap().status, Status::Interrupted);
        let events = engine.history("r1", 0);
        assert!(matches!(events.last().unwrap().kind, EventKind::RunInterrupted { .. }));
    }

    #[tokio::test]
    async fn submit_rejects_unknown_provider() {
        let home = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().to_path_buf(), Config::default());
        let req = RunRequest {
            repo: PathBuf::from("."),
            issue: "#1".into(),
            issue_text: None,
            provider: Some("nope".into()),
            policy_acknowledged: false,
        };
        assert!(engine.submit(req).is_err());
    }
}
