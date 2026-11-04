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
