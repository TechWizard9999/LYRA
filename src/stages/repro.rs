//! S2 Repro (PLAN §3.1): the agent writes one failing test in its own file; the harness checks
//! the diff against the allowed set, runs `cargo test -p <crate> <test_name> -- --exact`,
//! requires `FAILED` plus the recorded assertion text in the output, formats the file and
//! commits it. That commit is the S2 checkpoint and the frozen form of the test.

use super::{assets, Stage, StageCtx};
use crate::error::{ErrorKind, StageError, StageOutcome, StopReason};
use crate::events::StageId;
use crate::git;
use crate::guards::{self, ReproRecord, TestRun};
use crate::proc::{self, CommandSpec};
use crate::providers::Mode;
use crate::state::RunState;
use async_trait::async_trait;
use serde_json::json;
use std::path::Path;

pub struct ReproStage;

#[async_trait]
impl Stage for ReproStage {
    fn id(&self) -> StageId {
        StageId::Repro
    }
    async fn run(&self, ctx: &mut StageCtx) -> StageOutcome {
        run_inner(ctx).await.unwrap_or_else(StageOutcome::failed)
    }
}

async fn run_inner(ctx: &mut StageCtx) -> Result<StageOutcome, StageError> {
    let worktree = ctx.run_dir.worktree();
    let n = issue_number(&ctx.state);
    let base = ctx
        .state
        .base_commit
        .clone()
        .ok_or_else(|| StageError::fatal(ErrorKind::Git, "base commit not recorded by intake"))?;

    // 1. Ask the agent for a failing test.
    let values = json!({
        "issue": read_opt(ctx, "00-issue.md"),
        "context_summary": read_opt(ctx, "10-context/summary.json"),
        "instructions": read_opt(ctx, "10-context/instructions.md"),
        "repo_json": read_opt(ctx, "10-context/repo.json"),
        "issue_number": n,
    });
    let prompt = ctx
        .render("repro.md", assets::REPRO_PROMPT, values)
        .map_err(|e| StageError::fatal(ErrorKind::Config, format!("rendering repro prompt: {e:#}")))?;
    let task = ctx.agent_task(StageId::Repro, prompt, Mode::Edit, Some(assets::schema(assets::REPRO_SCHEMA)));
    ctx.events.info(format!("repro: asking the agent for a failing test for #{n}"));
    let agent = ctx.agent.clone();
    let events = ctx.events.clone();
    let out = agent.run(task, &events).await.map_err(|e| StageError::from_anyhow(ErrorKind::Provider, e))?;
    let transcript = rel_artifact(ctx, &out.transcript);
    if out.cancelled {
        return Err(StageError::new(ErrorKind::Tool, "repro cancelled"));
    }
    if out.timed_out {
        return Err(StageError::new(ErrorKind::Timeout, "agent call timed out").with_details(transcript));
    }
    if !out.ok {
        let msg = out.failure.clone().unwrap_or_else(|| "agent did not finish successfully".to_string());
        return Err(StageError::new(ErrorKind::Provider, msg).with_details(transcript));
    }
    let value = out.structured.clone().ok_or_else(|| {
        StageError::new(ErrorKind::Schema, "agent produced no structured repro output").with_details(&transcript)
    })?;
    let mut rec: ReproRecord = serde_json::from_value(value).map_err(|e| {
        StageError::new(ErrorKind::Schema, format!("repro output does not match the schema: {e}"))
            .with_details(&transcript)
    })?;

    // 2. Honest "cannot reproduce" is a stop, not an error.
    if !rec.reproduced {
        let notes = if rec.notes.trim().is_empty() {
            "the agent could not reproduce the issue".to_string()
        } else {
            rec.notes.trim().to_string()
        };
        let md = format!(
            "# Repro for #{n}: not reproduced\n\n{notes}\n\n## Observed\n\n```\n{}\n```\n",
            rec.observed.trim()
        );
        write(ctx, "20-repro/repro.md", md)?;
        ctx.events.warn(format!("repro: not reproduced: {notes}"));
        return Ok(StageOutcome::stopped(StopReason::NotReproduced, notes));
    }

    // Normalise the record; the command is ours, never the agent's.
    rec.krate = rec.krate.trim().to_string();
    rec.test_name = rec.test_name.trim().to_string();
    guards::validate_repro_record(&rec)
        .map_err(|msg| StageError::new(ErrorKind::Guard, msg).with_details(&transcript))?;
    rec.test_file = guards::norm_path(&rec.test_file);
    rec.registration_file = rec.registration_file.as_deref().map(guards::norm_path);
    rec.command = guards::repro_test_command(&rec.krate, &rec.test_name);
    // Fields supplied by the model cannot claim that a checkpoint already exists.
    rec.frozen_commit = None;
    rec.frozen_len = None;

    // 3. Only the allowed files may have changed.
    let changes = git::changed_files(&worktree, &base).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    if let Err(msg) = guards::check_repro_changes(&changes, &rec, &worktree, &base) {
        let listing = changes.iter().map(|(s, p)| format!("{s}\t{p}")).collect::<Vec<_>>().join("\n");
        let diff = git::diff(&worktree, &base, None).unwrap_or_default();
        let rel = write(
            ctx,
            "20-repro/changes.txt",
            format!("# changed files vs {}\n{listing}\n\n{diff}\n", git::short(&base)),
        )?;
        return Err(StageError::new(ErrorKind::Guard, msg).with_details(rel));
    }
    ctx.events.info(format!("repro: {} changed path(s), all within the allowed set", changes.len()));

    // Format before the baseline run so the committed test is the test that was executed.
    let abs_test = worktree.join(&rec.test_file);
    let fmt =
        CommandSpec::new("rustfmt", &worktree).args(["--edition", "2021"]).arg(abs_test.to_string_lossy()).scrubbed();
    let formatted =
        proc::run(&fmt, ctx.timeout(), &ctx.cancel).await.map_err(|e| StageError::from_anyhow(ErrorKind::Tool, e))?;
    if !formatted.success() {
        let log = write(ctx, "20-repro/format.txt", formatted.combined())?;
        return Err(StageError::new(ErrorKind::Tool, "formatting the repro test failed").with_details(log));
    }
    let tested_source = std::fs::read(&abs_test).map_err(|e| StageError::new(ErrorKind::Io, e.to_string()))?;

    // 4. Run exactly that test; selectors are validated and passed as individual args.
    let args =
        guards::repro_test_args(&rec.krate, &rec.test_name).map_err(|msg| StageError::new(ErrorKind::Guard, msg))?;
    let spec = CommandSpec::new("cargo", &worktree)
        .args(args)
        .scrubbed()
        .env("CARGO_TARGET_DIR", ctx.run_dir.root.join("repro-target").to_string_lossy());
    check_source_binding(ctx, &rec, spec.clone(), "20-repro/source-binding.txt").await?;
    ctx.events.info(format!("repro: running `{}`", spec.display()));
    let run =
        proc::run(&spec, ctx.timeout(), &ctx.cancel).await.map_err(|e| StageError::from_anyhow(ErrorKind::Tool, e))?;
    let output = run.combined();
    let out_rel = write(ctx, "20-repro/output.txt", &output)?;
    if run.cancelled {
        return Err(StageError::new(ErrorKind::Tool, "repro cancelled"));
    }
    if run.timed_out {
        let msg = format!("`{}` timed out after {}s", spec.display(), ctx.timeout().as_secs());
        return Err(StageError::new(ErrorKind::Timeout, msg).with_details(out_rel));
    }
    require_complete_evidence(&run).map_err(|error| error.with_details(&out_rel))?;
    match guards::classify_cargo_test(&output, &rec.test_name) {
        TestRun::Failed => {
            let summaries = guards::parse_summaries(&output);
            if run.exit_code != Some(101)
                || summaries.iter().map(|s| s.failed).sum::<u64>() != 1
                || summaries.iter().any(|s| s.passed != 0 || s.ignored != 0)
            {
                return Err(StageError::new(
                    ErrorKind::Guard,
                    "reproduction must execute exactly one failing test and exit with Cargo's test-failure status",
                )
                .with_details(out_rel));
            }
            if !guards::output_contains_expected(&output, &rec.expected) {
                let msg = format!(
                    "test {} failed, but its output does not contain the expected text {:?}",
                    rec.test_name,
                    rec.expected.trim()
                );
                return Err(StageError::new(ErrorKind::Guard, msg).with_details(out_rel));
            }
        }
        TestRun::Passed => {
            let msg = format!("test {} passed; a reproduction must fail on the recorded assertion", rec.test_name);
            return Err(StageError::new(ErrorKind::Guard, msg).with_details(out_rel));
        }
        other => {
            let msg = format!("not a reproduction: {}", guards::describe_test_run(&other, &rec.test_name));
            return Err(StageError::new(ErrorKind::Guard, msg).with_details(out_rel));
        }
    }
    ctx.events.info(format!("repro: test {} fails with the expected text", rec.test_name));

    // 5. Cargo/build scripts may have written files; guard that final diff before committing.
    if std::fs::read(&abs_test).map_err(|e| StageError::new(ErrorKind::Io, e.to_string()))? != tested_source {
        return Err(StageError::new(ErrorKind::Guard, "the reproduction test changed while it was executing"));
    }
    let changes = git::changed_files(&worktree, &base).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    guards::check_repro_changes(&changes, &rec, &worktree, &base)
        .map_err(|msg| StageError::new(ErrorKind::Guard, msg))?;
    let sha = git::commit_all(&worktree, &format!("test: failing repro for #{n}"))
        .map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    ctx.state.checkpoints.insert(StageId::Repro, sha.clone());
    rec.frozen_commit = Some(sha.clone());
    rec.frozen_len = std::fs::metadata(&abs_test).ok().map(|m| m.len());
    rec.observed = output;
    ctx.state.frozen_repro = Some(rec.clone());

    write_json(ctx, "20-repro/repro.json", &rec)?;
    write(ctx, "20-repro/repro.md", repro_markdown(&rec, n))?;
    ctx.events.info(format!("repro: committed {} (S2 checkpoint)", git::short(&sha)));
    Ok(StageOutcome::Completed)
}

fn require_complete_evidence(run: &proc::CommandOutput) -> Result<(), StageError> {
    if run.output_truncated || run.drain_timed_out {
        return Err(StageError::new(
            ErrorKind::Guard,
            "incomplete subprocess output cannot establish reproduction evidence",
        ));
    }
    Ok(())
}

/// Prove the exact selected test comes from the frozen source, rather than an
/// existing test or another target with the same name. `--list` compiles but never
/// executes the tests, and works for both integration and private-module tests.
pub(crate) async fn check_source_binding(
    ctx: &StageCtx,
    rec: &ReproRecord,
    spec: CommandSpec,
    log_rel: &str,
) -> Result<(), StageError> {
    let path = spec.cwd.join(&rec.test_file);
    let mut source = TemporarilyEmptySource::new(path)?;
    let listing_spec = source_binding_spec(&spec).arg("--list");
    ctx.events.info(format!("checking reproduction source binding: `{}`", listing_spec.display()));
    let run = proc::run(&listing_spec, ctx.timeout(), &ctx.cancel).await;
    // Restore before interpreting any subprocess failure, including cancellation.
    source.restore()?;
    let run = run.map_err(|e| StageError::from_anyhow(ErrorKind::Tool, e))?;
    let log = write(
        ctx,
        log_rel,
        format!(
            "Source temporarily emptied: {}\nCommand: {}\nThe discovery-only lint cap preserves active environment flags, or appends build.rustflags. Target-specific Cargo rustflags may take precedence; discovery then fails closed. Real test commands retain their original flags.\n\n{}",
            rec.test_file,
            listing_spec.display(),
            run.combined()
        ),
    )?;
    require_complete_evidence(&run).map_err(|error| error.with_details(&log))?;
    if !run.success() {
        let kind = if run.timed_out { ErrorKind::Timeout } else { ErrorKind::Guard };
        return Err(StageError::new(kind, "reproduction source binding could not be established: test discovery failed with the new source temporarily empty").with_details(log));
    }
    guards::assert_test_absent_from_list(&run.stdout, &rec.test_name)
        .map_err(|message| StageError::new(ErrorKind::Guard, message).with_details(log))
}

/// Emptying a test module can make existing test-only helpers/imports unused.
/// Cap lints only for discovery, preserving the active rustflags source. Cargo
/// merges the fallback array with build.rustflags; target-specific rustflags
/// still take precedence, so we fail closed rather than replace their cfg flags.
fn source_binding_spec(original: &CommandSpec) -> CommandSpec {
    let mut spec = original.clone();
    let active = |key: &str| {
        original
            .env
            .iter()
            .rev()
            .find(|(name, _)| name == key || (cfg!(windows) && name.eq_ignore_ascii_case(key)))
            .map(|(_, value)| value.clone())
            .or_else(|| (!original.clear_env).then(|| std::env::var(key).ok()).flatten())
    };
    if let Some(mut encoded) = active("CARGO_ENCODED_RUSTFLAGS") {
        if !encoded.is_empty() {
            encoded.push('\u{1f}');
        }
        encoded.push_str("--cap-lints=allow");
        spec.env.push(("CARGO_ENCODED_RUSTFLAGS".into(), encoded));
    } else if let Some(mut flags) = active("RUSTFLAGS") {
        if !flags.is_empty() {
            flags.push(' ');
        }
        flags.push_str("--cap-lints=allow");
        spec.env.push(("RUSTFLAGS".into(), flags));
    } else {
        spec.args.splice(0..0, ["--config".into(), "build.rustflags=[\"--cap-lints=allow\"]".into()]);
    }
    spec
}

struct TemporarilyEmptySource {
    path: std::path::PathBuf,
    original: Option<Vec<u8>>,
}

impl TemporarilyEmptySource {
    fn new(path: std::path::PathBuf) -> Result<Self, StageError> {
        let original = std::fs::read(&path).map_err(|e| StageError::new(ErrorKind::Io, e.to_string()))?;
        let source = Self { path, original: Some(original) };
        std::fs::write(&source.path, []).map_err(|e| StageError::new(ErrorKind::Io, e.to_string()))?;
        Ok(source)
    }

    fn restore(&mut self) -> Result<(), StageError> {
        if let Some(original) = self.original.as_ref() {
            std::fs::write(&self.path, original).map_err(|e| {
                StageError::new(ErrorKind::Io, format!("restoring reproduction source {}: {e}", self.path.display()))
            })?;
            self.original = None;
        }
        Ok(())
    }
}

impl Drop for TemporarilyEmptySource {
    fn drop(&mut self) {
        // Covers unwinding; ordinary exits surface restoration errors above.
        let _ = self.restore();
    }
}

fn repro_markdown(rec: &ReproRecord, n: u64) -> String {
    format!(
        "# Repro for #{n}\n\n\
         | field | value |\n|---|---|\n\
         | crate | `{}` |\n| test file | `{}` |\n| test name | `{}` |\n| command | `{}` |\n| registration | {} |\n| frozen commit | `{}` |\n\n\
         ## Expected (proves the bug)\n\n```\n{}\n```\n\n## Observed\n\n```\n{}\n```\n\n## Notes\n\n{}\n",
        rec.krate,
        rec.test_file,
        rec.test_name,
        rec.command,
        rec.registration_file.as_deref().map(|r| format!("`{r}`")).unwrap_or_else(|| "none (integration test)".to_string()),
        rec.frozen_commit.as_deref().unwrap_or("-"),
        rec.expected.trim(),
        rec.observed.trim(),
        if rec.notes.trim().is_empty() { "-" } else { rec.notes.trim() },
    )
}

// ---------------------------------------------------------------------------------------
// Helpers shared with the fix and verify stages.
// ---------------------------------------------------------------------------------------

/// Issue number from intake, or parsed from the request (`#123`, `.../issues/123`, `123`).
pub(crate) fn issue_number(state: &RunState) -> u64 {
    state.issue_number.unwrap_or_else(|| parse_issue_number(&state.request.issue).unwrap_or(0))
}

pub(crate) fn parse_issue_number(issue: &str) -> Option<u64> {
    let tail = issue.trim().trim_end_matches('/').rsplit(['/', '#']).next()?;
    tail.trim().parse().ok()
}

/// An optional artifact as text (empty when missing).
pub(crate) fn read_opt(ctx: &StageCtx, rel: &str) -> String {
    ctx.run_dir.read(rel).unwrap_or_default()
}

/// Path relative to `artifacts/` (forward slashes), for `StageError::details`.
pub(crate) fn rel_artifact(ctx: &StageCtx, path: &Path) -> String {
    match path.strip_prefix(ctx.run_dir.artifacts()) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

/// Writes an artifact and announces it; returns the relative path.
pub(crate) fn write(ctx: &StageCtx, rel: &str, contents: impl AsRef<[u8]>) -> Result<String, StageError> {
    let rel = ctx.run_dir.write(rel, contents).map_err(|e| StageError::from_anyhow(ErrorKind::Io, e))?;
    ctx.events.artifact(rel.clone());
    Ok(rel)
}

pub(crate) fn write_json<T: serde::Serialize>(ctx: &StageCtx, rel: &str, value: &T) -> Result<String, StageError> {
    let rel = ctx.run_dir.write_json(rel, value).map_err(|e| StageError::from_anyhow(ErrorKind::Io, e))?;
    ctx.events.artifact(rel.clone());
    Ok(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_baseline_output_is_not_reproduction_evidence() {
        for run in [
            proc::CommandOutput { output_truncated: true, ..Default::default() },
            proc::CommandOutput { drain_timed_out: true, ..Default::default() },
        ] {
            assert!(require_complete_evidence(&run).is_err());
        }
        assert!(require_complete_evidence(&proc::CommandOutput::default()).is_ok());
    }

    fn source_binding_fixture() -> (tempfile::TempDir, StageCtx, ReproRecord) {
        use crate::artifacts::RunDir;
        use crate::config::Config;
        use crate::events::EventSink;
        use crate::state::RunRequest;
        use tokio_util::sync::CancellationToken;
        let temporary = tempfile::tempdir().unwrap();
        let run_dir = RunDir::new(temporary.path().join("run"));
        run_dir.create().unwrap();
        let worktree = run_dir.worktree();
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::create_dir_all(worktree.join("tests")).unwrap();
        std::fs::write(
            worktree.join("Cargo.toml"),
            "[package]\nname = 'lyra_binding_fixture'\nversion = '0.1.0'\nedition = '2021'\n",
        )
        .unwrap();
        std::fs::write(worktree.join("src/lib.rs"), "#[test]\nfn existing_bug() { panic!(\"existing bug\"); }\n")
            .unwrap();
        let config = Config::default();
        let agent = crate::providers::make_agent("claude", config.provider("claude").unwrap());
        let state = RunState::new(
            "binding".into(),
            RunRequest {
                repo: worktree,
                issue: "1".into(),
                issue_text: None,
                provider: None,
                policy_acknowledged: false,
            },
            "claude".into(),
        );
        let ctx = StageCtx {
            run_dir,
            state,
            config,
            agent,
            events: EventSink::memory("binding"),
            cancel: CancellationToken::new(),
        };
        let record = ReproRecord {
            reproduced: true,
            krate: "lyra_binding_fixture".into(),
            test_file: "tests/issue_1.rs".into(),
            test_name: "existing_bug".into(),
            command: guards::repro_test_command("lyra_binding_fixture", "existing_bug"),
            expected: "existing bug".into(),
            observed: String::new(),
            registration_file: None,
            notes: String::new(),
            frozen_commit: None,
            frozen_len: None,
        };
        (temporary, ctx, record)
    }

    fn binding_spec(ctx: &StageCtx, record: &ReproRecord) -> CommandSpec {
        CommandSpec::new("cargo", ctx.run_dir.worktree())
            .args(guards::repro_test_args(&record.krate, &record.test_name).unwrap())
            .scrubbed()
            .env("CARGO_NET_OFFLINE", "true")
            .env("CARGO_TARGET_DIR", ctx.run_dir.root.join("target").to_string_lossy())
    }

    #[test]
    fn discovery_lint_cap_preserves_active_flags_and_leaves_strict_command_unchanged() {
        let process_flags = std::env::var_os("RUSTFLAGS");
        let process_encoded = std::env::var_os("CARGO_ENCODED_RUSTFLAGS");
        let mut strict = CommandSpec::new("cargo", ".").args(["test", "--", "--exact"]);
        strict.clear_env = true;
        strict.env.push(("RUSTFLAGS".into(), "--cfg keep_me -Dwarnings".into()));
        let before = strict.env.clone();
        let discovery = source_binding_spec(&strict);
        assert_eq!(
            discovery.env.last().unwrap(),
            &("RUSTFLAGS".into(), "--cfg keep_me -Dwarnings --cap-lints=allow".into())
        );
        assert_eq!(strict.env, before);
        assert_eq!(discovery.args, strict.args);

        strict.env.push(("CARGO_ENCODED_RUSTFLAGS".into(), "--cfg\u{1f}encoded_cfg\u{1f}-Dwarnings".into()));
        let before = strict.env.clone();
        let discovery = source_binding_spec(&strict);
        assert_eq!(
            discovery.env.last().unwrap(),
            &("CARGO_ENCODED_RUSTFLAGS".into(), "--cfg\u{1f}encoded_cfg\u{1f}-Dwarnings\u{1f}--cap-lints=allow".into())
        );
        assert_eq!(strict.env, before);
        strict.env.push(("CARGO_ENCODED_RUSTFLAGS".into(), String::new()));
        assert_eq!(source_binding_spec(&strict).env.last().unwrap().1, "--cap-lints=allow");

        strict.env.clear();
        let discovery = source_binding_spec(&strict);
        assert_eq!(&discovery.args[..2], &["--config", "build.rustflags=[\"--cap-lints=allow\"]"]);
        assert_eq!(&discovery.args[2..], &strict.args);
        assert!(strict.env.is_empty());
        assert_eq!(std::env::var_os("RUSTFLAGS"), process_flags);
        assert_eq!(std::env::var_os("CARGO_ENCODED_RUSTFLAGS"), process_encoded);
    }

    #[tokio::test]
    async fn source_binding_allows_preexisting_strict_test_only_helpers_without_relaxing_real_tests() {
        let (_temporary, ctx, mut record) = source_binding_fixture();
        std::fs::write(
            ctx.run_dir.worktree().join("src/lib.rs"),
            "#![deny(warnings)]\n#[cfg(test)]\nfn helper() -> i32 { 1 }\n#[cfg(test)]\nmod issue_1_tests;\n",
        )
        .unwrap();
        record.test_file = "src/issue_1_tests.rs".into();
        record.test_name = "issue_1_tests::regression".into();
        record.registration_file = Some("src/lib.rs".into());
        let path = ctx.run_dir.worktree().join(&record.test_file);
        std::fs::write(&path, "#[test]\nfn regression() { assert_eq!(super::helper(), 1); }\n").unwrap();
        let strict = binding_spec(&ctx, &record);
        let source = std::fs::read(&path).unwrap();
        check_source_binding(&ctx, &record, strict.clone(), "20-repro/source-binding.txt").await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), source);
        let run = proc::run(&strict, ctx.timeout(), &ctx.cancel).await.unwrap();
        assert!(run.success(), "{}", run.combined());
        assert!(guards::assert_test_passed(&run.combined(), &record.test_name).is_ok());
        // The unmodified command still enforces deny(warnings) after discovery.
        std::fs::write(&path, "#[test]\nfn regression() { let unused = super::helper(); }\n").unwrap();
        let strict_failure = proc::run(&strict, ctx.timeout(), &ctx.cancel).await.unwrap();
        assert!(!strict_failure.success());
        assert!(strict_failure.combined().contains("unused variable"), "{}", strict_failure.combined());
    }

    #[tokio::test]
    async fn existing_test_cannot_be_claimed_by_an_empty_new_source() {
        let (_temporary, ctx, record) = source_binding_fixture();
        let path = ctx.run_dir.worktree().join(&record.test_file);
        std::fs::write(&path, "// no regression test\r\n").unwrap();
        let before = std::fs::read(&path).unwrap();
        // Before the source-binding check, this record and empty new file passed S2's diff guard.
        assert!(guards::check_repro_changes_with(
            &[("??".into(), record.test_file.clone())],
            &record,
            &|_| None,
            &|_| Some(String::new())
        )
        .is_ok());
        let error = check_source_binding(&ctx, &record, binding_spec(&ctx, &record), "20-repro/source-binding.txt")
            .await
            .unwrap_err();
        assert!(error.message.contains("still discovered"), "{error:?}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn source_binding_accepts_integration_and_private_module_tests() {
        let (_temporary, ctx, mut record) = source_binding_fixture();
        let integration = ctx.run_dir.worktree().join(&record.test_file);
        std::fs::write(&integration, "#[test]\nfn new_regression() { panic!(\"new bug\"); }\n").unwrap();
        record.test_name = "new_regression".into();
        let before = std::fs::read(&integration).unwrap();
        check_source_binding(&ctx, &record, binding_spec(&ctx, &record), "20-repro/source-binding.txt").await.unwrap();
        assert_eq!(std::fs::read(&integration).unwrap(), before);

        std::fs::write(
            ctx.run_dir.worktree().join("src/lib.rs"),
            "fn private_value() -> i32 { 0 }\n#[cfg(test)]\nmod issue_1_tests;\n",
        )
        .unwrap();
        record.test_file = "src/issue_1_tests.rs".into();
        record.test_name = "issue_1_tests::private_regression".into();
        record.registration_file = Some("src/lib.rs".into());
        let private = ctx.run_dir.worktree().join(&record.test_file);
        std::fs::write(&private, "#[test]\nfn private_regression() { assert_eq!(super::private_value(), 1); }\n")
            .unwrap();
        let before = std::fs::read(&private).unwrap();
        check_source_binding(&ctx, &record, binding_spec(&ctx, &record), "20-repro/source-binding.txt").await.unwrap();
        assert_eq!(std::fs::read(&private).unwrap(), before);
    }

    #[tokio::test]
    async fn source_binding_restores_exact_bytes_on_failure_and_cancellation() {
        let (_temporary, ctx, record) = source_binding_fixture();
        let path = ctx.run_dir.worktree().join(&record.test_file);
        let before = b"// exact bytes\r\n\r\n";
        std::fs::write(&path, before).unwrap();
        let invalid = CommandSpec::new("lyra-missing-binding-command", ctx.run_dir.worktree());
        assert!(check_source_binding(&ctx, &record, invalid, "20-repro/source-binding.txt").await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        ctx.cancel.cancel();
        assert!(check_source_binding(&ctx, &record, binding_spec(&ctx, &record), "20-repro/source-binding.txt")
            .await
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn issue_number_parsing() {
        assert_eq!(parse_issue_number("#123"), Some(123));
        assert_eq!(parse_issue_number("123"), Some(123));
        assert_eq!(parse_issue_number("https://github.com/o/r/issues/42"), Some(42));
        assert_eq!(parse_issue_number("https://github.com/o/r/issues/42/"), Some(42));
        assert_eq!(parse_issue_number("pasted text"), None);
    }

    #[test]
    fn markdown_summarises_the_record() {
        let rec = ReproRecord {
            reproduced: true,
            krate: "foo".into(),
            test_file: "tests/issue_9.rs".into(),
            test_name: "issue_9_x".into(),
            command: guards::repro_test_command("foo", "issue_9_x"),
            expected: "boom".into(),
            observed: "it boomed".into(),
            registration_file: None,
            notes: String::new(),
            frozen_commit: Some("abc123".into()),
            frozen_len: Some(10),
        };
        let md = repro_markdown(&rec, 9);
        assert!(md.starts_with("# Repro for #9"));
        assert!(md.contains("`cargo test -p foo issue_9_x -- --exact`"));
        assert!(md.contains("none (integration test)"));
        assert!(md.contains("abc123"));
    }

    #[test]
    fn prompt_renders_with_and_without_context() {
        let mut env = minijinja::Environment::new();
        env.add_template("repro.md", assets::REPRO_PROMPT).unwrap();
        let t = env.get_template("repro.md").unwrap();
        let full = t
            .render(json!({"issue": "# Bug", "context_summary": "{\"crate\":\"foo\"}", "instructions": "use fmt", "repo_json": "{}", "issue_number": 7}))
            .unwrap();
        assert!(full.contains("tests/issue_7.rs"));
        assert!(full.contains("mod issue_7_tests;"));
        assert!(full.contains("Repository instructions"));
        assert!(full.contains("use fmt"));
        let bare = t.render(json!({"issue": "# Bug", "context_summary": "", "instructions": "", "repo_json": "", "issue_number": 7})).unwrap();
        assert!(!bare.contains("Repository instructions"));
        assert!(!bare.contains("Repository facts"));
        assert!(bare.contains("-- --exact"));
    }
}
