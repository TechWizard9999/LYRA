//! S5 Verify (PLAN §3.4), no agent: in a fresh detached checkout of the fix checkpoint, run the
//! profile's verification steps (build, the repro test which must pass as exactly one test, the
//! suite, the formatter). Logs go to `50-verify/`, the outcome to `result.json`; success records
//! `verified_commit = HEAD`. The harness owns the checkout, snapshots, timeouts and cleanup; the
//! execution profile owns the toolchain environment and commands.

use super::repro::{write, write_json};
use super::{Stage, StageCtx};
use crate::error::{ErrorKind, StageError, StageOutcome, StopReason};
use crate::events::StageId;
use crate::git;
use crate::guards::{self, ReproRecord};
use crate::proc;
use crate::profile::ExecutionProfile;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct VerifyStage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub name: String,
    pub command: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// Artifact path of the log, relative to `artifacts/`.
    pub log: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifyResult {
    pub ok: bool,
    pub steps: Vec<StepResult>,
    #[serde(default)]
    pub repository_overrides: bool,
}

#[async_trait]
impl Stage for VerifyStage {
    fn id(&self) -> StageId {
        StageId::Verify
    }
    async fn run(&self, ctx: &mut StageCtx) -> StageOutcome {
        run_inner(ctx).await.unwrap_or_else(StageOutcome::failed)
    }
}

async fn run_inner(ctx: &mut StageCtx) -> Result<StageOutcome, StageError> {
    ctx.state.verified_commit = None;
    let profile = ctx.profile()?;
    let rec = ctx.state.require_frozen_repro()?.clone();
    let candidate = ctx
        .state
        .checkpoints
        .get(&StageId::Fix)
        .cloned()
        .ok_or_else(|| StageError::new(ErrorKind::Guard, "fix checkpoint missing; retry from fix"))?;
    let source = ctx.run_dir.worktree();
    let head = git::head(&source).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    if head != candidate {
        return Err(StageError::new(ErrorKind::Guard, "worktree HEAD differs from the fix checkpoint; retry from fix"));
    }
    let suffix = format!(
        "{}-{}",
        ctx.state.attempt_count(StageId::Verify),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let root = verification_root(&ctx.run_dir.root, &suffix, &*profile)?;
    let worktree = root.join("checkout");
    // Own the root before any fallible setup, including worktree creation.
    let cleanup = VerificationWorktree {
        source: source.clone(),
        root: root.clone(),
        path: worktree.clone(),
        timeout: ctx.timeout(),
    };
    profile.create_environment(&root).map_err(|e| StageError::new(ErrorKind::Io, e.to_string()))?;
    git::verification_worktree(&source, &worktree, &candidate)
        .map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    ctx.events.info(format!("verify: isolated checkout at {}", worktree.display()));
    let outcome = verify_checkout(ctx, &*profile, &rec, &candidate, &worktree, &root).await;
    drop(cleanup);
    outcome
}

struct VerificationWorktree {
    source: PathBuf,
    root: PathBuf,
    path: PathBuf,
    timeout: Duration,
}

impl Drop for VerificationWorktree {
    fn drop(&mut self) {
        // The stage's token is already cancelled on the Cancel path. Cleanup must
        // still remove Git's registration and all verification caches/outputs.
        git::with_run_context(self.timeout, CancellationToken::new(), || {
            let _ = git::worktree_remove(&self.source, &self.path);
        });
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// An owned root for the checkout and the profile's environment, outside any ancestor the
/// profile rejects (for Cargo: inherited `.cargo/config`, which HOME and TEMP often carry).
fn verification_root(preferred: &Path, suffix: &str, profile: &dyn ExecutionProfile) -> Result<PathBuf, StageError> {
    let bases = vec![preferred.to_path_buf(), std::env::temp_dir()];
    #[cfg(windows)]
    let bases = {
        let mut bases = bases;
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            bases.push(PathBuf::from(system_root).join("Temp"));
        }
        bases
    };
    let mut candidates = bases.clone();
    for base in bases {
        for parent in base.ancestors() {
            if !candidates.iter().any(|candidate| candidate == parent) {
                candidates.push(parent.to_path_buf());
            }
        }
    }
    let mut failures = Vec::new();
    for parent in candidates {
        let parent = match dunce::canonicalize(&parent) {
            Ok(parent) => parent,
            Err(error) => {
                failures.push(format!("{}: {error}", parent.display()));
                continue;
            }
        };
        let root = parent.join(format!("lyra-verify-{}-{suffix}", std::process::id()));
        if profile.check_environment_root(&root.join("checkout")).is_err() {
            continue;
        }
        match std::fs::create_dir(&root) {
            Ok(()) => return Ok(root),
            Err(error) => failures.push(format!("{}: {error}", parent.display())),
        }
    }
    Err(StageError::new(
        ErrorKind::Config,
        format!(
            "cannot create a verification workspace outside inherited toolchain configuration; choose a writable LYRA_HOME outside those ancestors. {}",
            failures.join("; ")
        ),
    ))
}

fn guard(result: Result<(), String>) -> Result<(), StageError> {
    result.map_err(|message| StageError::new(ErrorKind::Guard, message))
}

async fn verify_checkout(
    ctx: &mut StageCtx,
    profile: &dyn ExecutionProfile,
    rec: &ReproRecord,
    candidate: &str,
    worktree: &Path,
    env_root: &Path,
) -> Result<StageOutcome, StageError> {
    guard(profile.check_environment_root(worktree))?;
    let checkpoint = rec.frozen_commit.as_deref().expect("validated frozen record");
    let changes = git::changed_files(worktree, checkpoint).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    guard(guards::check_fix_changes(&changes, rec, worktree, checkpoint, profile))?;
    guard(profile.check_environment(env_root))?;
    let snapshot = prepare_baseline(ctx, profile, candidate, worktree, env_root).await?;
    let binding = profile.regression_command(rec, worktree).map_err(|m| StageError::new(ErrorKind::Guard, m))?;
    super::repro::check_source_binding(
        ctx,
        profile,
        rec,
        profile.command_environment(binding, env_root),
        "50-verify/source-binding.txt",
    )
    .await?;
    assert_checkout_unchanged(profile, worktree, candidate, &snapshot)?;
    guard(profile.check_environment(env_root))?;

    let steps = profile
        .verification_steps(&ctx.config.verify, rec, worktree)
        .map_err(|message| StageError::fatal(ErrorKind::Config, message))?;
    let mut result = VerifyResult {
        repository_overrides: repository_verify_overrides(&ctx.state.request.repo)?,
        ..VerifyResult::default()
    };

    for step in steps {
        guard(profile.check_environment_root(worktree))?;
        guard(profile.check_environment(env_root))?;
        let spec = profile.command_environment(step.spec, env_root);
        ctx.events.info(format!("verify: {}: `{}`", step.name, spec.display()));
        let run = proc::run(&spec, ctx.timeout(), &ctx.cancel)
            .await
            .map_err(|e| StageError::from_anyhow(ErrorKind::Tool, e))?;
        let output = run.combined();
        let log = write(ctx, step.log, &output)?;
        let mut ok = run.success();
        let mut reason: Option<String> = None;
        if step.regression {
            if let Err(e) = profile.assert_passed(&output, rec) {
                ok = false;
                reason = Some(e);
            }
        }
        result.steps.push(StepResult {
            name: step.name.to_string(),
            command: spec.display(),
            ok: ok && !run.timed_out && !run.cancelled,
            exit_code: run.exit_code,
            duration_ms: run.duration.as_millis() as u64,
            log: log.clone(),
        });

        if run.cancelled {
            write_json(ctx, "50-verify/result.json", &result)?;
            return Err(StageError::new(ErrorKind::Tool, "verify cancelled"));
        }
        if run.timed_out {
            write_json(ctx, "50-verify/result.json", &result)?;
            let msg = format!("{} timed out after {}s: `{}`", step.name, ctx.timeout().as_secs(), spec.display());
            return Err(StageError::new(ErrorKind::Timeout, msg).with_details(log));
        }
        if let Err(error) = assert_checkout_unchanged(profile, worktree, candidate, &snapshot) {
            write_json(ctx, "50-verify/result.json", &result)?;
            return Err(error.with_details(log));
        }
        guard(profile.check_environment(env_root))?;
        if !ok {
            write_json(ctx, "50-verify/result.json", &result)?;
            let first = reason.unwrap_or_else(|| profile.explain_failure(&output, run.exit_code));
            ctx.events.warn(format!("verify: {} failed: {first}", step.name));
            return Ok(StageOutcome::stopped(StopReason::VerifyFailed, format!("{} failed: {first}", step.name)));
        }
        ctx.events.info(format!("verify: {} ok ({} ms)", step.name, run.duration.as_millis()));
    }

    result.ok = true;
    write_json(ctx, "50-verify/result.json", &result)?;
    ctx.state.verified_commit = Some(candidate.to_string());
    ctx.events.info(format!("verify: all steps passed in a fresh checkout; verified commit {}", git::short(candidate)));
    Ok(StageOutcome::Completed)
}

/// Freezes the checkout baseline. When the profile needs a generated input first (for Cargo:
/// a missing `Cargo.lock`), run that one preparation and require that it created nothing else;
/// the generated files are then protected like every other file.
async fn prepare_baseline(
    ctx: &StageCtx,
    profile: &dyn ExecutionProfile,
    candidate: &str,
    worktree: &Path,
    env_root: &Path,
) -> Result<SourceSnapshot, StageError> {
    let before = source_snapshot(worktree).map_err(|e| StageError::from_anyhow(ErrorKind::Io, e))?;
    let Some(prep) = profile.preparation(worktree, &|path| before.contains_key(path)) else {
        return Ok(before);
    };
    let spec = profile.command_environment(prep.spec, env_root);
    ctx.events.info(format!("verify: preparing {}: `{}`", prep.name, spec.display()));
    let run =
        proc::run(&spec, ctx.timeout(), &ctx.cancel).await.map_err(|e| StageError::from_anyhow(ErrorKind::Tool, e))?;
    let log = write(ctx, prep.log, run.combined())?;
    if !run.success() {
        let kind = if run.timed_out { ErrorKind::Timeout } else { ErrorKind::Tool };
        return Err(StageError::new(kind, format!("preparing the verification {} failed", prep.name)).with_details(log));
    }
    let mut after = source_snapshot(worktree).map_err(|e| StageError::from_anyhow(ErrorKind::Io, e))?;
    let mut generated = Vec::new();
    for path in &prep.creates {
        match after.remove(path) {
            Some(entry @ SourceEntry::File(_)) => generated.push((path.clone(), entry)),
            _ => {
                return Err(StageError::new(
                    ErrorKind::Guard,
                    format!("{} preparation did not create the regular file {}", prep.name, path.display()),
                )
                .with_details(log))
            }
        }
    }
    if after != before {
        return Err(StageError::new(
            ErrorKind::Guard,
            format!(
                "{} preparation must only create {}; candidate source or configuration changed",
                prep.name,
                prep.creates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            ),
        )
        .with_details(log));
    }
    after.extend(generated);
    assert_checkout_unchanged(profile, worktree, candidate, &after)?;
    guard(profile.check_environment(env_root))?;
    Ok(after)
}

fn repository_verify_overrides(repo: &Path) -> Result<bool, StageError> {
    let path = repo.join("lyra.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(StageError::new(ErrorKind::Config, format!("reading {}: {error}", path.display()))),
    };
    let value: toml::Value = toml::from_str(&text)
        .map_err(|error| StageError::new(ErrorKind::Config, format!("parsing {}: {error}", path.display())))?;
    Ok(value.get("verify").and_then(toml::Value::as_table).is_some_and(|table| !table.is_empty()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceEntry {
    File(Vec<u8>),
    Symlink(PathBuf),
}

pub(crate) type SourceSnapshot = BTreeMap<PathBuf, SourceEntry>;

/// Includes ignored/untracked files; build outputs and caches live outside this checkout.
pub(crate) fn source_snapshot(root: &Path) -> anyhow::Result<SourceSnapshot> {
    fn visit(root: &Path, directory: &Path, files: &mut SourceSnapshot) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if directory == root && entry.file_name() == ".git" {
                continue;
            }
            let kind = entry.file_type()?;
            let relative = path.strip_prefix(root)?.to_path_buf();
            if kind.is_symlink() {
                files.insert(relative, SourceEntry::Symlink(std::fs::read_link(path)?));
            } else if kind.is_dir() {
                visit(root, &path, files)?;
            } else if kind.is_file() {
                files.insert(relative, SourceEntry::File(std::fs::read(path)?));
            } else {
                anyhow::bail!("unsupported source entry {}", path.display());
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

fn assert_checkout_unchanged(
    profile: &dyn ExecutionProfile,
    root: &Path,
    candidate: &str,
    snapshot: &SourceSnapshot,
) -> Result<(), StageError> {
    guard(profile.check_environment_root(root))?;
    let head = git::head(root).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    let now = source_snapshot(root).map_err(|e| StageError::from_anyhow(ErrorKind::Io, e))?;
    if head != candidate || &now != snapshot {
        let changed = snapshot
            .keys()
            .chain(now.keys())
            .filter(|path| snapshot.get(*path) != now.get(*path))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .take(5)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(StageError::new(
            ErrorKind::Guard,
            format!("verification modified the candidate checkout ({changed}); refusing to certify an untested commit"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::rust_cargo::{self, RustCargo};

    #[test]
    fn snapshot_detects_source_and_ignored_config_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("source.rs"), "before").unwrap();
        let before = source_snapshot(dir.path()).unwrap();
        std::fs::write(dir.path().join("source.rs"), "after").unwrap();
        assert_ne!(source_snapshot(dir.path()).unwrap(), before);
        std::fs::write(dir.path().join("source.rs"), "before").unwrap();
        std::fs::create_dir(dir.path().join(".cargo")).unwrap();
        std::fs::write(dir.path().join(".cargo/config.toml"), "runner = 'fake'").unwrap();
        assert_ne!(source_snapshot(dir.path()).unwrap(), before);
    }

    #[test]
    fn generated_lockfile_remains_part_of_the_frozen_source_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "version = 4\n").unwrap();
        let before = source_snapshot(dir.path()).unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "version = 4\n# changed by a command\n").unwrap();
        assert_ne!(source_snapshot(dir.path()).unwrap(), before);
        std::fs::remove_file(dir.path().join("Cargo.lock")).unwrap();
        assert_ne!(source_snapshot(dir.path()).unwrap(), before);
    }

    #[test]
    fn verification_root_skips_ancestors_the_profile_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let preferred = dir.path().join("home/.lyra/run");
        std::fs::create_dir_all(&preferred).unwrap();
        std::fs::create_dir_all(dir.path().join("home/.cargo")).unwrap();
        std::fs::write(dir.path().join("home/.cargo/config.toml"), "[build]\n").unwrap();
        let root = verification_root(&preferred, "t", &RustCargo).unwrap();
        assert!(!root.starts_with(dunce::canonicalize(dir.path().join("home")).unwrap()), "{}", root.display());
        assert!(RustCargo.check_environment_root(&root.join("checkout")).is_ok());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cancelled_stage_still_cleans_worktree_registration_and_environment() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        std::fs::create_dir(&source).unwrap();
        git::run(&source, &["init", "-q"]).unwrap();
        git::run(&source, &["config", "user.name", "LYRA test"]).unwrap();
        git::run(&source, &["config", "user.email", "lyra-test@example.invalid"]).unwrap();
        std::fs::write(source.join("file.txt"), "candidate").unwrap();
        let candidate = git::commit_all(&source, "fixture").unwrap();
        let root = temporary.path().join("verification");
        let path = root.join("checkout");
        std::fs::create_dir_all(root.join("cargo-home/registry")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/output"), "large build output").unwrap();
        git::verification_worktree(&source, &path, &candidate).unwrap();
        let cleanup = VerificationWorktree {
            source: source.clone(),
            root: root.clone(),
            path: path.clone(),
            timeout: Duration::from_secs(10),
        };
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        git::with_run_context(Duration::from_secs(10), cancelled, || drop(cleanup));
        assert!(!root.exists(), "verification Cargo home and target must also be removed");
        assert!(!git::run(&source, &["worktree", "list", "--porcelain"])
            .unwrap()
            .contains(&path.to_string_lossy().replace('\\', "/")));
    }

    fn fixture_state(repository: &Path, id: &str, issue: &str) -> crate::state::RunState {
        use crate::state::{RunRequest, RunState};
        let mut state = RunState::new(
            id.into(),
            RunRequest {
                repo: repository.to_path_buf(),
                issue: issue.into(),
                issue_text: None,
                provider: None,
                policy_acknowledged: false,
            },
            "claude".into(),
        );
        state.profile = rust_cargo::detect(repository);
        state
    }

    #[tokio::test]
    async fn library_without_lockfile_verifies_outside_home_cargo_configuration() {
        use crate::artifacts::RunDir;
        use crate::config::Config;
        use crate::events::EventSink;

        let temporary = tempfile::tempdir().unwrap();
        let fake_home = temporary.path().join("home");
        std::fs::create_dir_all(fake_home.join(".cargo")).unwrap();
        std::fs::write(
            fake_home.join(".cargo/config.toml"),
            "[build]\nrustc-wrapper = 'this-ancestor-wrapper-must-not-run'\n",
        )
        .unwrap();
        let run_dir = RunDir::new(fake_home.join(".lyra/run"));
        run_dir.create().unwrap();
        let repository = run_dir.worktree();
        std::fs::create_dir_all(repository.join("src")).unwrap();
        std::fs::create_dir_all(repository.join("tests")).unwrap();
        std::fs::create_dir_all(repository.join(".cargo")).unwrap();
        git::run(&repository, &["init", "-q"]).unwrap();
        git::run(&repository, &["config", "user.name", "LYRA test"]).unwrap();
        git::run(&repository, &["config", "user.email", "lyra-test@example.invalid"]).unwrap();
        std::fs::write(
            repository.join("Cargo.toml"),
            "[package]\nname = 'lyra_verify_library'\nversion = '0.1.0'\nedition = '2021'\n",
        )
        .unwrap();
        std::fs::write(repository.join(".cargo/config.toml"), "[net]\noffline = true\n").unwrap();
        std::fs::write(repository.join(".gitignore"), "/Cargo.lock\n/target\n").unwrap();
        std::fs::write(repository.join("src/lib.rs"), "pub fn value() -> i32 {\n    0\n}\n").unwrap();
        std::fs::write(
            repository.join("tests/issue_7.rs"),
            "#[test]\nfn issue_7_expected() {\n    assert_eq!(lyra_verify_library::value(), 1, \"issue remains\");\n}\n",
        )
        .unwrap();
        let repro_commit = git::commit_all(&repository, "test: reproduce").unwrap();
        std::fs::write(repository.join("src/lib.rs"), "pub fn value() -> i32 {\n    1\n}\n").unwrap();
        let candidate = git::commit_all(&repository, "fix: expected value").unwrap();
        let record: ReproRecord = serde_json::from_value(serde_json::json!({
            "reproduced": true, "crate": "lyra_verify_library", "test_file": "tests/issue_7.rs",
            "test_name": "issue_7_expected", "expected": "issue remains",
            "command": rust_cargo::repro_test_command("lyra_verify_library", "issue_7_expected"),
            "frozen_commit": repro_commit, "profile": "rust-cargo"
        }))
        .unwrap();
        let config = Config::default();
        let agent = crate::providers::make_agent("claude", config.provider("claude").unwrap());
        let mut state = fixture_state(&repository, "library", "7");
        state.checkpoints.insert(StageId::Repro, record.frozen_commit.clone().unwrap());
        state.checkpoints.insert(StageId::Fix, candidate.clone());
        state.frozen_repro = Some(record);
        let mut ctx = StageCtx {
            run_dir,
            state,
            config,
            agent,
            events: EventSink::memory("library"),
            cancel: CancellationToken::new(),
        };
        assert!(RustCargo.check_environment_root(&repository).is_err());
        assert_eq!(run_inner(&mut ctx).await.unwrap(), StageOutcome::Completed);
        assert_eq!(ctx.state.verified_commit.as_deref(), Some(candidate.as_str()));
        assert!(ctx.run_dir.read("50-verify/lockfile.txt").is_ok());
        assert!(!repository.join("Cargo.lock").exists(), "the verified commit and agent checkout remain unchanged");
        let result: VerifyResult = ctx.run_dir.read_json("50-verify/result.json").unwrap();
        assert!(result.ok);
        assert_eq!(result.steps.len(), 4);
        assert_eq!(
            git::run(&repository, &["worktree", "list", "--porcelain"]).unwrap().matches("worktree ").count(),
            1
        );
    }

    #[tokio::test]
    async fn tampered_artifact_cannot_replace_frozen_failing_test() {
        use crate::artifacts::RunDir;
        use crate::config::Config;
        use crate::events::EventSink;

        let temporary = tempfile::tempdir().unwrap();
        let run_dir = RunDir::new(temporary.path().join("run"));
        run_dir.create().unwrap();
        let repository = run_dir.worktree();
        std::fs::create_dir_all(repository.join("src")).unwrap();
        std::fs::create_dir_all(repository.join("tests")).unwrap();
        git::run(&repository, &["init"]).unwrap();
        git::run(&repository, &["config", "user.name", "LYRA test"]).unwrap();
        git::run(&repository, &["config", "user.email", "lyra-test@example.invalid"]).unwrap();
        std::fs::write(
            repository.join("Cargo.toml"),
            "[package]\nname = \"lyra_verify_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            repository.join("Cargo.lock"),
            "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"lyra_verify_fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(repository.join(".gitignore"), "/target\n/.cargo/\n").unwrap();
        std::fs::write(repository.join("src/lib.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
        std::fs::write(repository.join("tests/issue_7.rs"),
            "#[test]\nfn issue_7_expected() { assert_eq!(lyra_verify_fixture::value(), 1, \"issue remains\"); }\n#[test]\nfn unrelated_pass() {}\n").unwrap();
        let repro_commit = git::commit_all(&repository, "test: reproduce").unwrap();
        std::fs::write(repository.join("src/lib.rs"), "pub fn value() -> i32 { 2 }\n").unwrap();
        let candidate = git::commit_all(&repository, "fix: deliberately incorrect").unwrap();

        let record: ReproRecord = serde_json::from_value(serde_json::json!({
            "reproduced": true, "crate": "lyra_verify_fixture", "test_file": "tests/issue_7.rs",
            "test_name": "issue_7_expected", "expected": "issue remains",
            "command": rust_cargo::repro_test_command("lyra_verify_fixture", "issue_7_expected"),
            "frozen_commit": repro_commit
        }))
        .unwrap();
        let mut tampered = record.clone();
        tampered.test_name = "unrelated_pass".into();
        tampered.command = rust_cargo::repro_test_command(&tampered.krate, &tampered.test_name);
        run_dir.write_json("20-repro/repro.json", &tampered).unwrap();
        // An ignored configuration in the agent's checkout must not reach verification.
        std::fs::create_dir_all(repository.join(".cargo")).unwrap();
        std::fs::write(
            repository.join(".cargo/config.toml"),
            "[build]\nrustc-wrapper = \"this-wrapper-does-not-exist\"\n",
        )
        .unwrap();

        let config = Config::default();
        let agent = crate::providers::make_agent("claude", config.provider("claude").unwrap());
        // A legacy run: no saved profile and no profile tag on the record.
        let mut state = fixture_state(&repository, "test-run", "7");
        state.profile = None;
        state.checkpoints.insert(StageId::Repro, record.frozen_commit.clone().unwrap());
        state.checkpoints.insert(StageId::Fix, candidate);
        state.frozen_repro = Some(record);
        let mut ctx = StageCtx {
            run_dir,
            state,
            config,
            agent,
            events: EventSink::memory("test-run"),
            cancel: CancellationToken::new(),
        };
        let outcome = run_inner(&mut ctx).await.unwrap();
        assert!(matches!(outcome, StageOutcome::Stopped { reason: StopReason::VerifyFailed, .. }), "{outcome:?}");
        assert!(ctx.state.verified_commit.is_none());
        let result: VerifyResult = ctx.run_dir.read_json("50-verify/result.json").unwrap();
        assert_eq!(result.steps.len(), 2);
        assert!(result.steps[0].ok);
        assert!(result.steps[1].command.contains("issue_7_expected"));
        assert!(!result.steps[1].command.contains("unrelated_pass"));
    }

    #[tokio::test]
    async fn redirected_parent_module_cannot_substitute_a_passing_test() {
        use crate::artifacts::RunDir;
        use crate::config::Config;
        use crate::events::EventSink;

        let temporary = tempfile::tempdir().unwrap();
        let run_dir = RunDir::new(temporary.path().join("run"));
        run_dir.create().unwrap();
        let repository = run_dir.worktree();
        std::fs::create_dir_all(repository.join("src/parent")).unwrap();
        git::run(&repository, &["init"]).unwrap();
        git::run(&repository, &["config", "user.name", "LYRA test"]).unwrap();
        git::run(&repository, &["config", "user.email", "lyra-test@example.invalid"]).unwrap();
        std::fs::write(
            repository.join("Cargo.toml"),
            "[package]\nname = 'lyra_redirect_fixture'\nversion = '0.1.0'\nedition = '2021'\n",
        )
        .unwrap();
        std::fs::write(
            repository.join("Cargo.lock"),
            "version = 4\n[[package]]\nname = 'lyra_redirect_fixture'\nversion = '0.1.0'\n",
        )
        .unwrap();
        std::fs::write(repository.join("src/lib.rs"), "pub mod parent;\n").unwrap();
        std::fs::write(
            repository.join("src/parent/mod.rs"),
            "fn value() -> i32 { 0 }\n#[cfg(test)]\nmod issue_7_tests;\n",
        )
        .unwrap();
        std::fs::write(
            repository.join("src/parent/issue_7_tests.rs"),
            "#[test]\nfn issue_7_expected() { assert_eq!(super::value(), 1); }\n",
        )
        .unwrap();
        // The replacement is already present but dormant at the repro checkpoint,
        // so adding duplicate declarations is not what this fixture exercises.
        std::fs::write(
            repository.join("src/replacement.rs"),
            "#[cfg(test)]\nmod issue_7_tests { #[test] fn issue_7_expected() {} }\n",
        )
        .unwrap();
        let repro_commit = git::commit_all(&repository, "test: reproduce").unwrap();
        std::fs::write(repository.join("src/lib.rs"), "#[path = \"replacement.rs\"]\npub mod parent;\n").unwrap();
        let candidate = git::commit_all(&repository, "fix: redirect the parent module").unwrap();
        let record: ReproRecord = serde_json::from_value(serde_json::json!({
            "reproduced": true, "crate": "lyra_redirect_fixture", "test_file": "src/parent/issue_7_tests.rs",
            "test_name": "parent::issue_7_tests::issue_7_expected", "registration_file": "src/parent/mod.rs",
            "expected": "assertion failed", "command": rust_cargo::repro_test_command("lyra_redirect_fixture", "parent::issue_7_tests::issue_7_expected"),
            "frozen_commit": repro_commit
        })).unwrap();
        // The source file and immediate registration are unchanged, so the old lock passes.
        let changes = git::changed_files(&repository, record.frozen_commit.as_deref().unwrap()).unwrap();
        guards::check_fix_changes(&changes, &record, &repository, record.frozen_commit.as_deref().unwrap(), &RustCargo)
            .unwrap();
        let config = Config::default();
        let agent = crate::providers::make_agent("claude", config.provider("claude").unwrap());
        let mut state = fixture_state(&repository, "redirect", "7");
        state.checkpoints.insert(StageId::Repro, record.frozen_commit.clone().unwrap());
        state.checkpoints.insert(StageId::Fix, candidate);
        state.frozen_repro = Some(record);
        let mut ctx = StageCtx {
            run_dir,
            state,
            config,
            agent,
            events: EventSink::memory("redirect"),
            cancel: CancellationToken::new(),
        };
        let error = run_inner(&mut ctx).await.unwrap_err();
        assert!(error.message.contains("still discovered"), "{error:?}");
        assert!(ctx.state.verified_commit.is_none());
    }

    #[test]
    fn result_json_shape() {
        let r = VerifyResult {
            ok: false,
            repository_overrides: false,
            steps: vec![StepResult {
                name: "build".into(),
                command: "cargo build".into(),
                ok: true,
                exit_code: Some(0),
                duration_ms: 12,
                log: "50-verify/build.txt".into(),
            }],
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["steps"][0]["name"], "build");
        assert_eq!(v["steps"][0]["exit_code"], 0);
        assert_eq!(v["steps"][0]["log"], "50-verify/build.txt");
    }
}
