//! S5 Verify (PLAN §3.4), no agent: build, run the repro test (it must pass, exactly one test),
//! the full test suite, then `cargo fmt --check`. Logs go to `50-verify/`, the outcome to
//! `result.json`; success records `verified_commit = HEAD`.

use super::repro::{write, write_json};
use super::{Stage, StageCtx};
use crate::error::{ErrorKind, StageError, StageOutcome, StopReason};
use crate::events::StageId;
use crate::git;
use crate::guards::{self, ReproRecord};
use crate::proc::{self, CommandSpec};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    let worktree = ctx.run_dir.root.join(format!("verify-{suffix}"));
    let environment = ctx.run_dir.root.join(format!("verify-env-{suffix}"));
    let cargo_home = environment.join("cargo-home");
    let target = environment.join("target");
    std::fs::create_dir_all(&cargo_home).map_err(|e| StageError::new(ErrorKind::Io, e.to_string()))?;
    git::verification_worktree(&source, &worktree, &candidate)
        .map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    let cleanup = VerificationWorktree { source, path: worktree.clone() };
    let outcome = verify_checkout(ctx, &rec, &candidate, &worktree, &cargo_home, &target).await;
    drop(cleanup);
    outcome
}

struct VerificationWorktree {
    source: PathBuf,
    path: PathBuf,
}

impl Drop for VerificationWorktree {
    fn drop(&mut self) {
        let _ = git::worktree_remove(&self.source, &self.path);
    }
}

async fn verify_checkout(
    ctx: &mut StageCtx,
    rec: &ReproRecord,
    candidate: &str,
    worktree: &Path,
    cargo_home: &Path,
    target: &Path,
) -> Result<StageOutcome, StageError> {
    ensure_no_ancestor_cargo_config(worktree)?;
    let checkpoint = rec.frozen_commit.as_deref().expect("validated frozen record");
    let changes = git::changed_files(worktree, checkpoint).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    guards::check_fix_changes(&changes, rec, worktree, checkpoint)
        .map_err(|msg| StageError::new(ErrorKind::Guard, msg))?;
    let snapshot = source_snapshot(worktree).map_err(|e| StageError::from_anyhow(ErrorKind::Io, e))?;

    let steps: [(&str, String, &str); 4] = [
        ("build", ctx.config.verify.build.clone(), "50-verify/build.txt"),
        ("repro", guards::repro_test_command(&rec.krate, &rec.test_name), "50-verify/repro-after.txt"),
        ("tests", ctx.config.verify.test.clone(), "50-verify/tests.txt"),
        ("fmt", ctx.config.verify.fmt.clone(), "50-verify/fmt.txt"),
    ];
    let mut result = VerifyResult {
        repository_overrides: repository_verify_overrides(&ctx.state.request.repo)?,
        ..VerifyResult::default()
    };

    for (name, line, log_rel) in steps {
        ensure_no_ancestor_cargo_config(worktree)?;
        ensure_empty_cargo_config(cargo_home)?;
        let spec = if name == "repro" {
            CommandSpec::new("cargo", worktree).args(
                guards::repro_test_args(&rec.krate, &rec.test_name)
                    .map_err(|msg| StageError::new(ErrorKind::Guard, msg))?,
            )
        } else {
            CommandSpec::from_line(&line, worktree).map_err(|_| {
                StageError::fatal(ErrorKind::Config, format!("verify step '{name}' has an empty command"))
            })?
        };
        let spec = controlled_cargo_environment(spec, cargo_home, target);
        ctx.events.info(format!("verify: {name}: `{}`", spec.display()));
        let run = proc::run(&spec, ctx.timeout(), &ctx.cancel)
            .await
            .map_err(|e| StageError::from_anyhow(ErrorKind::Tool, e))?;
        let output = run.combined();
        let log = write(ctx, log_rel, &output)?;
        let mut ok = run.success();
        let mut reason: Option<String> = None;
        if name == "repro" {
            if let Err(e) = guards::assert_test_passed(&output, &rec.test_name) {
                ok = false;
                reason = Some(e);
            }
        }
        result.steps.push(StepResult {
            name: name.to_string(),
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
            let msg = format!("{name} timed out after {}s: `{}`", ctx.timeout().as_secs(), spec.display());
            return Err(StageError::new(ErrorKind::Timeout, msg).with_details(log));
        }
        if let Err(error) = assert_checkout_unchanged(worktree, candidate, &snapshot) {
            write_json(ctx, "50-verify/result.json", &result)?;
            return Err(error.with_details(log));
        }
        ensure_empty_cargo_config(cargo_home)?;
        if !ok {
            write_json(ctx, "50-verify/result.json", &result)?;
            let first = reason.unwrap_or_else(|| first_error_line(&output, run.exit_code));
            ctx.events.warn(format!("verify: {name} failed: {first}"));
            return Ok(StageOutcome::stopped(StopReason::VerifyFailed, format!("{name} failed: {first}")));
        }
        ctx.events.info(format!("verify: {name} ok ({} ms)", run.duration.as_millis()));
    }

    result.ok = true;
    write_json(ctx, "50-verify/result.json", &result)?;
    ctx.state.verified_commit = Some(candidate.to_string());
    ctx.events.info(format!("verify: all steps passed in a fresh checkout; verified commit {}", git::short(candidate)));
    Ok(StageOutcome::Completed)
}

fn controlled_cargo_environment(spec: CommandSpec, cargo_home: &Path, target: &Path) -> CommandSpec {
    let mut spec = spec.scrubbed();
    spec.env
        .retain(|(key, _)| !key.eq_ignore_ascii_case("CARGO_HOME") && !key.eq_ignore_ascii_case("CARGO_TARGET_DIR"));
    spec.env.push(("CARGO_HOME".into(), cargo_home.to_string_lossy().into_owned()));
    spec.env.push(("CARGO_TARGET_DIR".into(), target.to_string_lossy().into_owned()));
    spec
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

fn ensure_empty_cargo_config(cargo_home: &Path) -> Result<(), StageError> {
    if ["config", "config.toml"].iter().any(|name| std::fs::symlink_metadata(cargo_home.join(name)).is_ok()) {
        return Err(StageError::new(ErrorKind::Guard, "verification Cargo home contains unexpected configuration"));
    }
    Ok(())
}

fn ensure_no_ancestor_cargo_config(worktree: &Path) -> Result<(), StageError> {
    for ancestor in worktree.parent().into_iter().flat_map(Path::ancestors) {
        for name in ["config", "config.toml"] {
            let path = ancestor.join(".cargo").join(name);
            if std::fs::symlink_metadata(&path).is_ok() {
                return Err(StageError::new(
                    ErrorKind::Guard,
                    format!(
                        "verification would inherit Cargo configuration from {}; move LYRA_HOME outside that directory",
                        path.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum SourceEntry {
    File(Vec<u8>),
    Symlink(PathBuf),
}

type SourceSnapshot = BTreeMap<PathBuf, SourceEntry>;

/// Includes ignored/untracked files; target and Cargo caches live outside this checkout.
fn source_snapshot(root: &Path) -> anyhow::Result<SourceSnapshot> {
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

fn assert_checkout_unchanged(root: &Path, candidate: &str, snapshot: &SourceSnapshot) -> Result<(), StageError> {
    ensure_no_ancestor_cargo_config(root)?;
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

/// The line that best explains a failed command: the first compiler error, panic, rustfmt
/// diff header or `test ... FAILED`; otherwise the exit code.
fn first_error_line(output: &str, exit_code: Option<i32>) -> String {
    for line in output.lines().map(str::trim) {
        if line.starts_with("error")
            || line.contains("panicked at")
            || line.starts_with("Diff in ")
            || (line.starts_with("test ") && line.ends_with("FAILED"))
        {
            return line.to_string();
        }
    }
    match exit_code {
        Some(c) => format!("exit code {c}"),
        None => "process was killed".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controlled_cargo_paths_override_inherited_configuration() {
        let home = Path::new("controlled-cargo-home");
        let target = Path::new("controlled-target");
        let spec = controlled_cargo_environment(
            CommandSpec::new("cargo", ".")
                .env("CARGO_HOME", "untrusted-home")
                .env("CARGO_TARGET_DIR", "untrusted-target"),
            home,
            target,
        );
        assert!(spec.clear_env);
        assert_eq!(
            spec.env.iter().filter(|(k, _)| k.eq_ignore_ascii_case("CARGO_HOME")).collect::<Vec<_>>(),
            vec![&("CARGO_HOME".into(), "controlled-cargo-home".into())]
        );
        assert!(!spec.env.iter().any(|(_, value)| value.starts_with("untrusted-")));
    }

    #[test]
    fn ancestor_and_injected_cargo_configuration_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("run").join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::create_dir_all(dir.path().join(".cargo")).unwrap();
        std::fs::write(dir.path().join(".cargo/config.toml"), "[build]\nrustc-wrapper = 'fake'").unwrap();
        assert!(ensure_no_ancestor_cargo_config(&checkout).unwrap_err().message.contains("inherit Cargo"));
        let home = dir.path().join("cargo-home");
        std::fs::create_dir_all(&home).unwrap();
        assert!(ensure_empty_cargo_config(&home).is_ok());
        std::fs::write(home.join("config"), "[alias]\ntest = 'fake'").unwrap();
        assert!(ensure_empty_cargo_config(&home).is_err());
    }

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

    #[tokio::test]
    async fn tampered_artifact_cannot_replace_frozen_failing_test() {
        use crate::artifacts::RunDir;
        use crate::config::Config;
        use crate::events::EventSink;
        use crate::state::{RunRequest, RunState};
        use tokio_util::sync::CancellationToken;

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
            "command": guards::repro_test_command("lyra_verify_fixture", "issue_7_expected"),
            "frozen_commit": repro_commit
        }))
        .unwrap();
        let mut tampered = record.clone();
        tampered.test_name = "unrelated_pass".into();
        tampered.command = guards::repro_test_command(&tampered.krate, &tampered.test_name);
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
        let mut state = RunState::new(
            "test-run".into(),
            RunRequest {
                repo: repository,
                issue: "7".into(),
                issue_text: None,
                provider: None,
                policy_acknowledged: false,
            },
            "claude".into(),
        );
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

    #[test]
    fn first_error_line_picks_the_explaining_line() {
        let build =
            "   Compiling foo v0.1.0\nwarning: unused import\nerror[E0308]: mismatched types\n --> src/x.rs:1:1\n";
        assert_eq!(first_error_line(build, Some(101)), "error[E0308]: mismatched types");
        let tests = "running 2 tests\ntest a ... ok\ntest b ... FAILED\n\nfailures:\n\n---- b stdout ----\nthread 'b' panicked at src/x.rs:3:5:\nboom\n";
        assert_eq!(first_error_line(tests, Some(101)), "test b ... FAILED");
        let fmt = "Diff in C:\\src\\foo\\src\\x.rs:12:\n-    a\n+a\n";
        assert!(first_error_line(fmt, Some(1)).starts_with("Diff in "));
        assert_eq!(first_error_line("nothing useful\n", Some(3)), "exit code 3");
        assert_eq!(first_error_line("", None), "process was killed");
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
