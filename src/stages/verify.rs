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
    let worktree = ctx.run_dir.worktree();
    let rec: ReproRecord = ctx
        .run_dir
        .read_json("20-repro/repro.json")
        .map_err(|e| StageError::fatal(ErrorKind::Io, format!("reading 20-repro/repro.json: {e:#}")))?;
    if rec.test_name.trim().is_empty() || rec.krate.trim().is_empty() {
        return Err(StageError::fatal(ErrorKind::Io, "20-repro/repro.json has no crate/test_name"));
    }
    let repro_cmd = if rec.command.trim().is_empty() {
        guards::repro_test_command(&rec.krate, &rec.test_name)
    } else {
        rec.command.clone()
    };

    let steps: [(&str, String, &str); 4] = [
        ("build", ctx.config.verify.build.clone(), "50-verify/build.txt"),
        ("repro", repro_cmd, "50-verify/repro-after.txt"),
        ("tests", ctx.config.verify.test.clone(), "50-verify/tests.txt"),
        ("fmt", ctx.config.verify.fmt.clone(), "50-verify/fmt.txt"),
    ];
    let mut result = VerifyResult::default();

    for (name, line, log_rel) in steps {
        let spec = CommandSpec::from_line(&line, &worktree)
            .map_err(|_| StageError::fatal(ErrorKind::Config, format!("verify step '{name}' has an empty command")))?
            .scrubbed();
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
    let head = git::head(&worktree).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    ctx.state.verified_commit = Some(head.clone());
    ctx.events.info(format!("verify: all steps passed; verified commit {}", git::short(&head)));
    Ok(StageOutcome::Completed)
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
