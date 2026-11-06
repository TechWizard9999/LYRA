//! The engine: creates runs, drives the fixed pipeline one stage at a time, persists
//! `run.json` and `events.jsonl` on every transition, and implements cancel and
//! retry-from-stage (archive outputs, restore the git checkpoint, invalidate later stages).
//! One active run at a time.

use crate::artifacts::RunDir;
use crate::config::{runs_dir, Config};
use crate::error::{ErrorKind, StageError, StageOutcome};
use crate::events::{read_events, Event, EventKind, EventSink, StageId};
use crate::git;
use crate::providers::{make_agent, SharedAgent};
use crate::stages::{pipeline, StageCtx};
use crate::state::{now_rfc3339, Attempt, AttemptResult, RunId, RunRequest, RunState, StageStatus, Status};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Shared senders survive retry while subscribed; unused inactive sinks are evicted.
    sinks: Mutex<HashMap<RunId, EventSink>>,
    admission: Mutex<()>,
    writer: Mutex<Option<File>>,
}

impl Engine {
    pub fn new(home: PathBuf, config: Config) -> Arc<Self> {
        Arc::new(Self {
            home,
            config,
            active: Mutex::new(HashMap::new()),
            sinks: Mutex::new(HashMap::new()),
            admission: Mutex::new(()),
            writer: Mutex::new(None),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn run_dir(&self, id: &str) -> Result<RunDir> {
        if id.is_empty() || id.len() > 200 || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
            bail!("invalid run id");
        }
        let root = runs_dir(&self.home).join(id);
        if std::fs::symlink_metadata(&root).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!("run directory must not be a symlink");
        }
        Ok(RunDir::new(root))
    }

    fn ensure_writer(&self) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        if writer.is_none() {
            std::fs::create_dir_all(&self.home)?;
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(self.home.join("writer.lock"))?;
            fs2::FileExt::try_lock_exclusive(&file)
                .context("another LYRA process owns this home; stop it before starting a writer")?;
            *writer = Some(file);
        }
        Ok(())
    }

    pub fn is_active(&self, id: &str) -> bool {
        self.active.lock().unwrap().contains_key(id)
    }

    pub fn cancel_all(&self) {
        for active in self.active.lock().unwrap().values() {
            active.cancel.cancel();
        }
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
        static NEXT: AtomicU64 = AtomicU64::new(0);
        format!(
            "{}-{}-{}-{}-{}-{}",
            clean(&repo_name).chars().take(50).collect::<String>(),
            clean(&issue).chars().take(20).collect::<String>(),
            ts,
            std::process::id(),
            time::OffsetDateTime::now_utc().nanosecond(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Creates the run directory and state, then starts the pipeline in the background.
    pub fn submit(self: &Arc<Self>, req: RunRequest) -> Result<RunId> {
        let _admission = self.admission.try_lock().map_err(|_| anyhow::anyhow!("another run is being admitted"))?;
        self.ensure_writer()?;
        if self.is_busy() {
            bail!("a run is already active; wait for it to finish or cancel it");
        }
        let provider = req.provider.clone().unwrap_or_else(|| self.config.run.provider.clone());
        self.config.provider(&provider)?;
        let id = Self::new_run_id(&req);
        let rd = self.run_dir(&id)?;
        std::fs::create_dir_all(runs_dir(&self.home))?;
        std::fs::create_dir(&rd.root).context("run id already exists; refusing to overwrite it")?;
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
        self.retry_with_policy_ack(id, from, false)
    }

    /// Retry while optionally acknowledging the policy discovered during intake.
    /// False preserves an earlier acknowledgment; it never revokes one.
    pub fn retry_with_policy_ack(self: &Arc<Self>, id: &str, from: StageId, acknowledge_policy: bool) -> Result<()> {
        let _admission = self.admission.try_lock().map_err(|_| anyhow::anyhow!("another run is being admitted"))?;
        self.ensure_writer()?;
        if self.active.lock().unwrap().contains_key(id) {
            bail!("run {id} is still active");
        }
        if self.is_busy() {
            bail!("another run is active");
        }
        let rd = self.run_dir(id)?;
        let mut state = RunState::load(&rd.root).with_context(|| format!("loading run {id}"))?;
        if !state.status.is_terminal() {
            bail!("run {id} is not in a terminal state");
        }
        if let Some(earlier) = StageId::ALL
            .iter()
            .copied()
            .take_while(|stage| *stage < from)
            .find(|stage| state.stage_status(*stage) != StageStatus::Done)
        {
            bail!("cannot retry from {from}: earlier stage {earlier} has not completed successfully; retry from {earlier} or an earlier stage");
        }
        if from == StageId::Intake {
            // Intake recreates the worktree; nothing to restore.
        } else {
            let cp = state
                .checkpoint_before(from)
                .cloned()
                .context("retry requires an earlier checkpoint; retry from intake")?;
            let wt = rd.worktree();
            if !wt.is_dir() {
                bail!("retry worktree is missing; retry from intake");
            }
            git::with_run_context(self.config.timeout(), CancellationToken::new(), || {
                git::rev_parse(&wt, &cp).context("retry checkpoint is missing")?;
                git::reset_hard_clean(&wt, &cp).with_context(|| format!("restoring checkpoint {}", git::short(&cp)))
            })?;
        }
        let n = state.attempts.len() as u32 + 1;
        rd.archive_from(from, n)?;
        state.invalidate_from(from);
        state.request.policy_acknowledged |= acknowledge_policy;
        state.save(&rd.root)?;
        let sink = self.sink_for(id)?;
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
        RunState::load(&self.run_dir(id)?.root)
    }

    /// All runs, newest first.
    pub fn list(&self) -> Vec<RunState> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(runs_dir(&self.home)) {
            for e in rd.flatten() {
                let name = e.file_name();
                let Some(id) = name.to_str() else { continue };
                let Ok(directory) = self.run_dir(id) else { continue };
                if !e.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                if let Ok(st) = RunState::load(&directory.root) {
                    out.push(st);
                }
            }
        }
        out.sort_by(|a, b| b.created.cmp(&a.created));
        out
    }

    /// Live subscription (None if the run does not exist).
    pub fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<Event>> {
        if !self.run_dir(id).ok()?.root.is_dir() {
            return None;
        }
        let active = self.active.lock().unwrap();
        if let Some(a) = active.get(id) {
            return Some(a.sink.subscribe());
        }
        // Keep the mutex through subscription so eviction cannot replace this sender.
        // Always acquire active before sinks, and preserve active writers even when
        // they currently have no receivers.
        let mut sinks = self.sinks.lock().unwrap();
        sinks.retain(|key, sink| key == id || active.contains_key(key) || sink.receiver_count() > 0);
        if !sinks.contains_key(id) {
            sinks.insert(
                id.into(),
                EventSink::open(id, &crate::events::events_path(&self.run_dir(id).ok()?.root)).ok()?,
            );
        }
        Some(sinks.get(id)?.subscribe())
    }

    fn sink_for(&self, id: &str) -> Result<EventSink> {
        let active = self.active.lock().unwrap();
        let mut sinks = self.sinks.lock().unwrap();
        sinks.retain(|key, sink| key == id || active.contains_key(key) || sink.receiver_count() > 0);
        if let Some(sink) = sinks.get(id) {
            return Ok(sink.clone());
        }
        let sink = EventSink::open(id, &crate::events::events_path(&self.run_dir(id)?.root))?;
        sinks.insert(id.into(), sink.clone());
        Ok(sink)
    }

    /// Events so far (replay), optionally after `since` seq.
    pub fn history(&self, id: &str, since: u64) -> Vec<Event> {
        let Ok(rd) = self.run_dir(id) else {
            return Vec::new();
        };
        read_events(&crate::events::events_path(&rd.root))
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.seq > since)
            .collect()
    }

    /// On startup: any run left `running`/`queued` by a previous process is marked interrupted.
    pub fn mark_interrupted(&self) -> Result<usize> {
        let _admission = self.admission.lock().unwrap();
        self.ensure_writer()?;
        let mut n = 0;
        for mut st in self.list() {
            if !st.status.is_terminal() && !self.is_active(&st.id) {
                st.status = Status::Interrupted;
                st.finished = Some(now_rfc3339());
                if let Some(a) = st.attempts.iter_mut().rev().find(|a| matches!(a.result, AttemptResult::Running)) {
                    a.result = AttemptResult::Cancelled;
                    a.finished = Some(now_rfc3339());
                }
                let rd = self.run_dir(&st.id)?;
                st.save(&rd.root)?;
                if let Ok(sink) = EventSink::open(&st.id, &crate::events::events_path(&rd.root)) {
                    sink.emit(EventKind::RunInterrupted { stage: Some(st.stage) });
                }
                n += 1;
            }
        }
        Ok(n)
    }

    fn spawn(self: &Arc<Self>, id: RunId, state: RunState, sink: EventSink) {
        let cancel = CancellationToken::new();
        self.active.lock().unwrap().insert(id.clone(), Active { cancel: cancel.clone(), sink: sink.clone() });
        let engine = Arc::clone(self);
        self.sinks.lock().unwrap().insert(id.clone(), sink.clone());
        tokio::spawn(async move {
            // A separate task converts stage panics into the same persisted terminal failure as errors.
            let worker = Arc::clone(&engine);
            let worker_sink = sink.clone();
            let fallback = state.clone();
            let result = tokio::spawn(async move { worker.drive(state, worker_sink, cancel).await }).await;
            engine.finish_worker(&id, fallback, &sink, result);
        });
    }

    fn finish_worker(
        &self,
        id: &str,
        fallback: RunState,
        sink: &EventSink,
        result: std::result::Result<Result<()>, tokio::task::JoinError>,
    ) {
        if let Err(e) = result.unwrap_or_else(|e| Err(anyhow::anyhow!("engine task failed: {e}"))) {
            let mut state = self.get(id).unwrap_or(fallback);
            let error = StageError::new(ErrorKind::Io, format!("engine error: {e:#}"));
            state.status = Status::Failed;
            state.finished = Some(now_rfc3339());
            state.last_error = Some(error.clone());
            let stage = state.stage;
            state.set_stage_status(stage, StageStatus::Failed);
            finish_attempt(&mut state, AttemptResult::Failed { error: error.clone() });
            if let Err(save) = self.run_dir(id).and_then(|rd| state.save(&rd.root)) {
                sink.warn(format!("saving terminal state: {save:#}"));
            }
            sink.emit(EventKind::StageFailed { stage, attempt: state.attempt_count(stage), error });
        }
        let mut active = self.active.lock().unwrap();
        active.remove(id);
        self.sinks.lock().unwrap().retain(|id, sink| active.contains_key(id) || sink.receiver_count() > 0);
    }

    async fn drive(self: &Arc<Self>, mut state: RunState, sink: EventSink, cancel: CancellationToken) -> Result<()> {
        let rd = self.run_dir(&state.id)?;
        let config = self.config.clone().with_repo_overrides(&state.request.repo)?;
        sink.info(format!(
            "effective verification: build={}; test={}; fmt={}",
            config.verify.build, config.verify.test, config.verify.fmt
        ));
        let pcfg = config.provider(&state.provider)?.clone();
        let agent: SharedAgent = make_agent(&state.provider, &pcfg);

        state.status = Status::Running;
        state.save(&rd.root)?;

        let stages = pipeline();
        let start_idx = stages.iter().position(|s| s.id() == state.stage).unwrap_or(0);
        let mut ctx =
            StageCtx { run_dir: rd.clone(), state, config, agent, events: sink.clone(), cancel: cancel.clone() };

        for stage in stages.into_iter().skip(start_idx) {
            let id = stage.id();
            if cancel.is_cancelled() {
                break;
            }
            let attempt_n = ctx.state.attempt_count(id) + 1;
            let timeout = ctx.config.timeout();
            let head_cancel = cancel.clone();
            let worktree = rd.worktree();
            let input_commit = tokio::task::spawn_blocking(move || {
                git::with_run_context(timeout, head_cancel, || git::head(&worktree).ok())
            })
            .await
            .context("reading input commit")?;
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

            let handle = tokio::runtime::Handle::current();
            let stage_cancel = cancel.clone();
            let (next_ctx, outcome) = tokio::task::spawn_blocking(move || {
                git::with_run_context(timeout, stage_cancel, || {
                    let outcome = handle.block_on(stage.run(&mut ctx));
                    (ctx, outcome)
                })
            })
            .await
            .context("stage task panicked")?;
            ctx = next_ctx;
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
            profile: None,
        };
        let id = Engine::new_run_id(&req);
        assert!(id.starts_with("my-crate-42-"), "{id}");
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn list_and_mark_interrupted() {
        let home = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().to_path_buf(), Config::default());
        let rd = engine.run_dir("r1").unwrap();
        rd.create().unwrap();
        let req = RunRequest {
            repo: PathBuf::from("."),
            issue: "#1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
            profile: None,
        };
        let mut st = RunState::new("r1".into(), req, "claude".into());
        st.status = Status::Running;
        st.save(&rd.root).unwrap();
        assert_eq!(engine.list().len(), 1);
        assert_eq!(engine.mark_interrupted().unwrap(), 1);
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
            profile: None,
        };
        assert!(engine.submit(req).is_err());
    }

    #[test]
    fn writer_lock_and_run_paths_are_enforced() {
        let home = tempfile::tempdir().unwrap();
        let one = Engine::new(home.path().into(), Config::default());
        let two = Engine::new(home.path().into(), Config::default());
        one.mark_interrupted().unwrap();
        assert!(two.mark_interrupted().is_err());
        for id in ["", "../outside", "a/b", "a\\b", "C:foo", "."] {
            assert!(one.run_dir(id).is_err());
        }
        drop(one);
        two.mark_interrupted().unwrap();
    }

    #[test]
    fn list_rejects_invalid_directory_names() {
        let home = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().into(), Config::default());
        let directory = runs_dir(home.path()).join("invalid.name");
        std::fs::create_dir_all(&directory).unwrap();
        let request = RunRequest {
            repo: PathBuf::from("."),
            issue: "1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
            profile: None,
        };
        RunState::new("invalid.name".into(), request, "claude".into()).save(&directory).unwrap();
        assert!(engine.list().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn list_does_not_follow_run_directory_symlinks() {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().into(), Config::default());
        let request = RunRequest {
            repo: PathBuf::from("."),
            issue: "1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
            profile: None,
        };
        RunState::new("linked".into(), request, "claude".into()).save(outside.path()).unwrap();
        std::fs::create_dir_all(runs_dir(home.path())).unwrap();
        std::os::unix::fs::symlink(outside.path(), runs_dir(home.path()).join("linked")).unwrap();
        assert!(engine.list().is_empty());
        assert_eq!(engine.mark_interrupted().unwrap(), 0);
    }

    #[test]
    fn run_ids_are_unique_in_a_burst() {
        let req = RunRequest {
            repo: PathBuf::from("repo"),
            issue: "1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
            profile: None,
        };
        let ids: std::collections::HashSet<_> = (0..1000).map(|_| Engine::new_run_id(&req)).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[tokio::test]
    async fn malformed_repo_config_finishes_and_releases_run() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("lyra.toml"), "[invalid").unwrap();
        let engine = Engine::new(home.path().into(), Config::default());
        let req = RunRequest {
            repo: repo.path().into(),
            issue: "1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
            profile: None,
        };
        let id = engine.submit(req).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while engine.is_busy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(engine.get(&id).unwrap().status, Status::Failed);
        assert!(matches!(engine.history(&id, 0).last().unwrap().kind, EventKind::StageFailed { .. }));
    }

    #[test]
    fn retry_without_checkpoint_does_not_invalidate_state() {
        let home = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().into(), Config::default());
        let rd = engine.run_dir("retry").unwrap();
        rd.create().unwrap();
        let req = RunRequest {
            repo: PathBuf::from("repo"),
            issue: "1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
            profile: None,
        };
        let mut st = RunState::new("retry".into(), req, "claude".into());
        st.status = Status::Failed;
        for stage in StageId::ALL.into_iter().take_while(|stage| *stage < StageId::Fix) {
            st.set_stage_status(stage, StageStatus::Done);
        }
        st.save(&rd.root).unwrap();
        assert!(engine.retry("retry", StageId::Fix).unwrap_err().to_string().contains("checkpoint"));
        assert_eq!(engine.get("retry").unwrap().status, Status::Failed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retry_cannot_skip_unresolved_rca() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        git::run(repo.path(), &["init", "-q", "-b", "main"]).unwrap();
        git::run(repo.path(), &["config", "user.name", "Fixture"]).unwrap();
        git::run(repo.path(), &["config", "user.email", "fixture@example.com"]).unwrap();
        std::fs::write(repo.path().join("source.rs"), "fn example() {}\n").unwrap();
        let base = git::commit_all(repo.path(), "fixture").unwrap();
        let engine = Engine::new(home.path().into(), Config::default());
        let rd = engine.run_dir("unresolved").unwrap();
        rd.create().unwrap();
        git::worktree_add(repo.path(), &rd.worktree(), "lyra/unresolved", &base).unwrap();
        let mut state = RunState::new("unresolved".into(), request(repo.path()), "claude".into());
        state.base_commit = Some(base.clone());
        state.checkpoints.insert(StageId::Repro, base.clone());
        for stage in [StageId::Intake, StageId::Context, StageId::Repro] {
            state.set_stage_status(stage, StageStatus::Done);
        }
        state.stage = StageId::Rca;
        state.set_stage_status(StageId::Rca, StageStatus::Stopped);
        state.status = Status::Stopped;
        state.save(&rd.root).unwrap();
        rd.write("40-fix/notes.md", "preserve previous attempt").unwrap();
        let error = engine
            .retry_with_policy_ack("unresolved", StageId::Fix, true)
            .expect_err("retry must not skip unresolved RCA");
        assert!(error.to_string().contains("rca"), "{error:#}");
        assert_eq!(engine.get("unresolved").unwrap().status, Status::Stopped);
        assert!(!engine.get("unresolved").unwrap().request.policy_acknowledged);
        assert_eq!(rd.read("40-fix/notes.md").unwrap(), "preserve previous attempt");
        assert_eq!(git::head(&rd.worktree()).unwrap(), base);
        assert!(!engine.is_busy());
        git::worktree_remove(repo.path(), &rd.worktree()).unwrap();
    }

    fn request(repo: &Path) -> RunRequest {
        RunRequest {
            repo: repo.into(),
            issue: "1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
            profile: None,
        }
    }

    fn seed_failed(engine: &Engine, id: &str, repo: &Path) -> RunState {
        let rd = engine.run_dir(id).unwrap();
        rd.create().unwrap();
        let mut state = RunState::new(id.into(), request(repo), "claude".into());
        state.status = Status::Failed;
        state.save(&rd.root).unwrap();
        state
    }

    async fn wait_idle(engine: &Engine) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while engine.is_busy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retry_saves_policy_acknowledgment_and_preserves_existing_choice() {
        for (previous, acknowledge, expected) in [(false, false, false), (false, true, true), (true, false, true)] {
            let home = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            git::run(repo.path(), &["init", "-q", "-b", "main"]).unwrap();
            git::run(repo.path(), &["config", "user.name", "Fixture"]).unwrap();
            git::run(repo.path(), &["config", "user.email", "fixture@example.com"]).unwrap();
            std::fs::write(repo.path().join("source.rs"), "fn example() {}\n").unwrap();
            let candidate = git::commit_all(repo.path(), "fixture").unwrap();
            let engine = Engine::new(home.path().into(), Config::default());
            let rd = engine.run_dir("policy").unwrap();
            rd.create().unwrap();
            git::worktree_add(repo.path(), &rd.worktree(), "lyra/policy", &candidate).unwrap();
            let mut state = RunState::new("policy".into(), request(repo.path()), "claude".into());
            state.request.policy_acknowledged = previous;
            state.has_policy = true;
            state.status = Status::Stopped;
            state.stage = StageId::Pr;
            state.base_commit = Some(candidate.clone());
            state.checkpoints.insert(StageId::Fix, candidate.clone());
            state.verified_commit = Some(candidate);
            state.stop_reason = Some(crate::error::StopReason::PolicyNotAcknowledged);
            for stage in StageId::ALL.into_iter().take_while(|stage| *stage < StageId::Pr) {
                state.set_stage_status(stage, StageStatus::Done);
            }
            state.save(&rd.root).unwrap();
            engine.retry_with_policy_ack("policy", StageId::Pr, acknowledge).unwrap();
            // The current-thread driver has not run yet; admission already persisted the choice.
            assert_eq!(engine.get("policy").unwrap().request.policy_acknowledged, expected);
            wait_idle(&engine).await;
            let state = engine.get("policy").unwrap();
            assert_eq!(state.request.policy_acknowledged, expected);
            if expected {
                // No GitHub origin is configured: reaching this guard proves the policy gate
                // was passed without calling gh or a provider in this offline fixture.
                assert_eq!(state.status, Status::Failed);
                assert!(state.last_error.unwrap().message.contains("origin is not a GitHub repository"));
                assert_ne!(state.stop_reason, Some(crate::error::StopReason::PolicyNotAcknowledged));
            } else {
                assert_eq!(state.status, Status::Stopped);
                assert_eq!(state.stop_reason, Some(crate::error::StopReason::PolicyNotAcknowledged));
            }
            git::worktree_remove(repo.path(), &rd.worktree()).unwrap();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn simultaneous_submissions_and_retries_admit_one_writer() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("lyra.toml"), "[invalid").unwrap();
        let engine = Engine::new(home.path().into(), Config::default());
        let runtime = tokio::runtime::Handle::current();
        let barrier = std::sync::Barrier::new(8);
        // On the current-thread runtime the admitted driver cannot finish before
        // every caller has returned, making the concurrency assertion deterministic.
        let results = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        let _entered = runtime.enter();
                        barrier.wait();
                        engine.submit(request(repo.path()))
                    })
                })
                .collect();
            threads.into_iter().map(|t| t.join().unwrap()).collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(engine.list().len(), 1);
        let id = results.into_iter().find_map(Result::ok).unwrap();
        wait_idle(&engine).await;
        let barrier = std::sync::Barrier::new(8);
        let results = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        let _entered = runtime.enter();
                        barrier.wait();
                        engine.retry(&id, StageId::Intake)
                    })
                })
                .collect();
            threads.into_iter().map(|t| t.join().unwrap()).collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        wait_idle(&engine).await;
        assert_eq!(
            engine.history(&id, 0).iter().filter(|e| matches!(e.kind, EventKind::RetryRequested { .. })).count(),
            1
        );
    }

    #[tokio::test]
    async fn a_subscriber_stays_on_the_same_sender_across_retry() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("lyra.toml"), "[invalid").unwrap();
        let engine = Engine::new(home.path().into(), Config::default());
        seed_failed(&engine, "retry", repo.path());
        let mut rx = engine.subscribe("retry").unwrap();
        engine.retry("retry", StageId::Intake).unwrap();
        assert!(matches!(rx.try_recv().unwrap().kind, EventKind::RetryRequested { .. }));
        wait_idle(&engine).await;
        let mut terminal = false;
        while let Ok(event) = rx.try_recv() {
            terminal |= matches!(event.kind, EventKind::StageFailed { .. });
        }
        assert!(terminal, "pre-retry receiver must receive retry completion");
    }

    #[test]
    fn browsing_finished_runs_evicts_unused_sinks_but_preserves_active_senders() {
        let home = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().into(), Config::default());
        seed_failed(&engine, "active", home.path());
        let sink = engine.sink_for("active").unwrap();
        engine
            .active
            .lock()
            .unwrap()
            .insert("active".into(), Active { cancel: CancellationToken::new(), sink: sink.clone() });
        for n in 0..20 {
            let id = format!("finished-{n}");
            seed_failed(&engine, &id, home.path());
            drop(engine.subscribe(&id).unwrap());
            assert!(engine.sinks.lock().unwrap().len() <= 2);
            assert!(engine.sinks.lock().unwrap().contains_key("active"));
        }
        let mut rx = engine.subscribe("active").unwrap();
        engine.active.lock().unwrap().remove("active");
        engine.sink_for("active").unwrap().info("next attempt");
        assert!(matches!(rx.try_recv().unwrap().kind, EventKind::Log { .. }));
    }

    #[tokio::test]
    async fn supervisor_turns_a_worker_panic_into_a_retryable_terminal_state() {
        let home = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().into(), Config::default());
        let mut state = seed_failed(&engine, "panic", home.path());
        state.status = Status::Running;
        state.set_stage_status(StageId::Intake, StageStatus::Running);
        state.save(&engine.run_dir("panic").unwrap().root).unwrap();
        let sink = engine.sink_for("panic").unwrap();
        let mut rx = sink.subscribe();
        engine
            .active
            .lock()
            .unwrap()
            .insert("panic".into(), Active { cancel: CancellationToken::new(), sink: sink.clone() });
        let result = tokio::spawn(async {
            panic!("test stage panic");
            #[allow(unreachable_code)]
            Ok(())
        })
        .await;
        engine.finish_worker("panic", state, &sink, result);
        assert!(!engine.is_busy());
        let state = engine.get("panic").unwrap();
        assert_eq!(state.status, Status::Failed);
        assert_eq!(state.stage_status(StageId::Intake), StageStatus::Failed);
        assert!(state.last_error.unwrap().message.contains("test stage panic"));
        assert!(matches!(rx.try_recv().unwrap().kind, EventKind::StageFailed { .. }));
    }
}
