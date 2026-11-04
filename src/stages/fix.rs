//! S4 Fix (PLAN §3.3): the agent changes production code to make the locked test pass. The
//! harness rejects any change to the frozen test, its registration line, `Cargo.toml` test
//! sections or `.cargo/`, records the scope of the diff, and commits (the S4 checkpoint).

use super::repro::{issue_number, read_opt, rel_artifact, write, write_json};
use super::{assets, Stage, StageCtx};
use crate::error::{ErrorKind, StageError, StageOutcome};
use crate::events::StageId;
use crate::git;
use crate::guards;
use crate::providers::Mode;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub struct FixStage;

/// The agent's structured answer (`schemas/fix.schema.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FixRecord {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    files_changed: Vec<String>,
    #[serde(default)]
    notes: String,
}

#[async_trait]
impl Stage for FixStage {
    fn id(&self) -> StageId {
        StageId::Fix
    }
    async fn run(&self, ctx: &mut StageCtx) -> StageOutcome {
        run_inner(ctx).await.unwrap_or_else(StageOutcome::failed)
    }
}

async fn run_inner(ctx: &mut StageCtx) -> Result<StageOutcome, StageError> {
    let worktree = ctx.run_dir.worktree();
    let n = issue_number(&ctx.state);

    // 1. Inputs from earlier stages.
    let rec = ctx.state.require_frozen_repro()?.clone();
    let rca_json = ctx
        .run_dir
        .read("30-rca/rca.json")
        .map_err(|e| StageError::fatal(ErrorKind::Io, format!("reading 30-rca/rca.json: {e:#}")))?;
    let checkpoint = ctx
        .state
        .checkpoints
        .get(&StageId::Repro)
        .cloned()
        .ok_or_else(|| StageError::fatal(ErrorKind::Git, "repro checkpoint missing; retry from repro"))?;
    let culprits = culprit_files(&rca_json);
    let issue_md = read_opt(ctx, "00-issue.md");

    // 2. Ask the agent for the fix.
    let values = json!({
        "issue": issue_md,
        "rca_json": rca_json,
        "repro_json": serde_json::to_string_pretty(&rec).unwrap_or_default(),
        "instructions": read_opt(ctx, "10-context/instructions.md"),
        "test_file": rec.test_file,
        "test_name": rec.test_name,
        "command": rec.command,
        "crate_name": rec.krate,
        "issue_number": n,
    });
    let prompt = ctx
        .render("fix.md", assets::FIX_PROMPT, values)
        .map_err(|e| StageError::fatal(ErrorKind::Config, format!("rendering fix prompt: {e:#}")))?;
    let task = ctx.agent_task(StageId::Fix, prompt, Mode::Edit, Some(assets::schema(assets::FIX_SCHEMA)));
    ctx.events.info(format!("fix: asking the agent to make {} pass", rec.test_name));
    let agent = ctx.agent.clone();
    let events = ctx.events.clone();
    let out = agent.run(task, &events).await.map_err(|e| StageError::from_anyhow(ErrorKind::Provider, e))?;
    let transcript = rel_artifact(ctx, &out.transcript);
    if out.cancelled {
        return Err(StageError::new(ErrorKind::Tool, "fix cancelled"));
    }
    if out.timed_out {
        return Err(StageError::new(ErrorKind::Timeout, "agent call timed out").with_details(transcript));
    }
    if !out.ok {
        let msg = out.failure.clone().unwrap_or_else(|| "agent did not finish successfully".to_string());
        return Err(StageError::new(ErrorKind::Provider, msg).with_details(transcript));
    }
    let answer: FixRecord = match out.structured.clone() {
        Some(v) => serde_json::from_value(v).map_err(|e| {
            StageError::new(ErrorKind::Schema, format!("fix output does not match the schema: {e}"))
                .with_details(&transcript)
        })?,
        None => {
            return Err(
                StageError::new(ErrorKind::Schema, "agent produced no structured fix output").with_details(&transcript)
            )
        }
    };

    // 3. The test lock.
    let changes = git::changed_files(&worktree, &checkpoint).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    if changes.is_empty() {
        return Err(StageError::new(ErrorKind::Guard, "fix produced no changes").with_details(transcript));
    }
    if let Err(msg) = guards::check_fix_changes(&changes, &rec, &worktree, &checkpoint) {
        let listing = changes.iter().map(|(s, p)| format!("{s}\t{p}")).collect::<Vec<_>>().join("\n");
        let diff = git::diff(&worktree, &checkpoint, None).unwrap_or_default();
        let rel = write(
            ctx,
            "40-fix/rejected.diff",
            format!("# changed files vs {}\n{listing}\n\n{diff}\n", git::short(&checkpoint)),
        )?;
        return Err(StageError::new(ErrorKind::Guard, msg).with_details(rel));
    }
    ctx.events.info(format!("fix: {} changed path(s), test lock intact", changes.len()));

    // 4. Diff, scope and notes. Staging first makes new files part of the diff.
    git::run(&worktree, &["add", "-A"]).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    let diff = git::diff(&worktree, &checkpoint, None).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    write(ctx, "40-fix/fix.diff", format!("{diff}\n"))?;
    let scope = guards::scope_from_diff(&diff, &culprits);
    for w in &scope.warnings {
        ctx.events.warn(format!("fix: {w}"));
    }
    write_json(ctx, "40-fix/scope.json", &scope)?;
    write(ctx, "40-fix/notes.md", notes_markdown(&answer, &scope))?;

    // 5. Commit: the S4 checkpoint.
    let title = issue_title(&issue_md).unwrap_or_else(|| format!("issue #{n}"));
    let sha = git::commit_all(&worktree, &format!("fix: {title} (#{n})"))
        .map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    ctx.state.checkpoints.insert(StageId::Fix, sha.clone());
    ctx.events.info(format!(
        "fix: committed {} (S4 checkpoint): +{} -{} in {} file(s)",
        git::short(&sha),
        scope.total_added,
        scope.total_removed,
        scope.files.len()
    ));
    Ok(StageOutcome::Completed)
}

/// `culprits[].file` from `rca.json` (empty when absent or malformed).
fn culprit_files(rca_json: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(rca_json)
        .ok()
        .and_then(|v| v.get("culprits").and_then(|c| c.as_array()).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| c.get("file").and_then(|f| f.as_str()).map(guards::norm_path))
        .filter(|f| !f.is_empty())
        .collect()
}

/// The issue title from the first markdown heading of `00-issue.md`, without a leading
/// `#123:` / `Issue #123` or a trailing `(#123)`.
fn issue_title(issue_md: &str) -> Option<String> {
    let heading = issue_md.lines().map(str::trim).find(|l| l.starts_with('#'))?;
    let text = heading.trim_start_matches('#').trim();
    let lead = regex::Regex::new(r"(?i)^(issue\s*)?#?\d+\s*[:\-–—]?\s*").ok()?;
    let trail = regex::Regex::new(r"\s*\(#\d+\)\s*$").ok()?;
    let text = trail.replace(&lead.replace(text, ""), "").trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn notes_markdown(answer: &FixRecord, scope: &guards::Scope) -> String {
    let list = |items: &[String]| {
        if items.is_empty() {
            "- (none)\n".to_string()
        } else {
            items.iter().map(|i| format!("- {i}\n")).collect()
        }
    };
    format!(
        "# Fix notes\n\n## Summary\n\n{}\n\n## Files changed (as reported by the agent)\n\n{}\n## Notes\n\n{}\n\n## Scope\n\n{} file(s), +{} -{} lines\n\n{}",
        if answer.summary.trim().is_empty() { "-" } else { answer.summary.trim() },
        list(&answer.files_changed),
        if answer.notes.trim().is_empty() { "-" } else { answer.notes.trim() },
        scope.files.len(),
        scope.total_added,
        scope.total_removed,
        if scope.warnings.is_empty() { String::new() } else { format!("Warnings:\n\n{}", list(&scope.warnings)) },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn culprits_are_read_from_rca_json() {
        let rca = r#"{"result":"supported","culprits":[{"file":"src/parser.rs","symbol":"parse_header","lines":[1,2]},{"file":"./src/lib.rs"},{"symbol":"x"}]}"#;
        assert_eq!(culprit_files(rca), vec!["src/parser.rs".to_string(), "src/lib.rs".to_string()]);
        assert!(culprit_files("not json").is_empty());
        assert!(culprit_files(r#"{"result":"unresolved"}"#).is_empty());
    }

    #[test]
    fn issue_title_from_first_heading() {
        assert_eq!(issue_title("# Panic on short frame\n\nbody").as_deref(), Some("Panic on short frame"));
        assert_eq!(issue_title("intro\n\n## #123: Panic on short frame\n").as_deref(), Some("Panic on short frame"));
        assert_eq!(
            issue_title("# Issue #123 - Panic on short frame (#123)\n").as_deref(),
            Some("Panic on short frame")
        );
        assert_eq!(issue_title("# 123\n"), None);
        assert_eq!(issue_title("no heading"), None);
        assert_eq!(issue_title(""), None);
    }

    #[test]
    fn notes_markdown_lists_everything() {
        let answer = FixRecord {
            summary: "Fixed bounds check".into(),
            files_changed: vec!["src/parser.rs".into()],
            notes: String::new(),
        };
        let scope = guards::scope_from_diff(
            "diff --git a/src/parser.rs b/src/parser.rs\n@@ -1 +1 @@\n-a\n+b\n",
            &["src/other.rs".into()],
        );
        let md = notes_markdown(&answer, &scope);
        assert!(md.contains("Fixed bounds check"));
        assert!(md.contains("- src/parser.rs"));
        assert!(md.contains("1 file(s), +1 -1 lines"));
        assert!(md.contains("not named by the RCA"));
    }

    #[test]
    fn fix_record_is_lenient() {
        let r: FixRecord = serde_json::from_str(r#"{"summary":"s"}"#).unwrap();
        assert_eq!(r.summary, "s");
        assert!(r.files_changed.is_empty());
    }

    #[test]
    fn prompt_renders() {
        let mut env = minijinja::Environment::new();
        env.add_template("fix.md", assets::FIX_PROMPT).unwrap();
        let text = env
            .get_template("fix.md")
            .unwrap()
            .render(json!({"issue": "# Bug", "rca_json": "{}", "repro_json": "{}", "instructions": "", "test_file": "tests/issue_3.rs",
                "test_name": "issue_3_x", "command": "cargo test -p foo issue_3_x -- --exact", "crate_name": "foo", "issue_number": 3}))
            .unwrap();
        assert!(text.contains("Do NOT modify `tests/issue_3.rs`"));
        assert!(text.contains("mod issue_3_tests;"));
        assert!(text.contains("cargo test -p foo"));
        assert!(!text.contains("Repository instructions"));
    }
}
