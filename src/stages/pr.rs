//! S6 Draft PR: push the verified commit to the run's branch and find-or-create a draft PR
//! whose head is exactly that commit. No model call.

use super::{assets, Stage, StageCtx};
use crate::error::{ErrorKind, StageError, StageOutcome, StopReason};
use crate::events::StageId;
use crate::{git, github};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct PrStage;

#[async_trait]
impl Stage for PrStage {
    fn id(&self) -> StageId {
        StageId::Pr
    }
    async fn run(&self, ctx: &mut StageCtx) -> StageOutcome {
        match pr(ctx) {
            Ok(o) => o,
            Err(e) => StageOutcome::failed(e),
        }
    }
}

fn gh_err(e: anyhow::Error) -> StageError {
    StageError::from_anyhow(ErrorKind::GitHub, e)
}

fn pr(ctx: &mut StageCtx) -> Result<StageOutcome, StageError> {
    if ctx.state.has_policy && !ctx.state.request.policy_acknowledged {
        return Ok(StageOutcome::stopped(
            StopReason::PolicyNotAcknowledged,
            "the repository has a contribution policy (policy.md): tick the policy checkbox / pass --ack-policy, then retry",
        ));
    }
    let wt = ctx.run_dir.worktree();
    let verified = ctx
        .state
        .verified_commit
        .clone()
        .ok_or_else(|| StageError::fatal(ErrorKind::Guard, "no verified commit recorded; run verify first"))?;
    let head = git::head(&wt).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    if head != verified {
        return Err(StageError::new(
            ErrorKind::Guard,
            format!("worktree HEAD {} is not the verified commit {}", git::short(&head), git::short(&verified)),
        ));
    }
    let slug =
        ctx.state.repo_slug.clone().ok_or_else(|| {
            StageError::fatal(ErrorKind::GitHub, "origin is not a GitHub repository; cannot open a PR")
        })?;
    let branch =
        ctx.state.branch.clone().ok_or_else(|| StageError::fatal(ErrorKind::Git, "no branch recorded by intake"))?;

    // Fork-aware target.
    let rv = github::repo_view(&slug).map_err(gh_err)?;
    let (target, head_owner, head_ref, base) = if rv.is_fork {
        let parent = rv
            .parent
            .clone()
            .ok_or_else(|| StageError::new(ErrorKind::GitHub, format!("{slug} is a fork but gh reports no parent")))?;
        let user = github::gh_user().map_err(gh_err)?;
        let base = github::repo_view(&parent).map(|p| p.default_branch).unwrap_or_else(|_| rv.default_branch.clone());
        (parent, user.clone(), format!("{user}:{branch}"), base)
    } else if matches!(rv.viewer_permission.as_str(), "ADMIN" | "MAINTAIN" | "WRITE") {
        let owner = slug.split('/').next().unwrap_or_default().to_string();
        (slug.clone(), owner, branch.clone(), rv.default_branch.clone())
    } else {
        return Ok(StageOutcome::stopped(
            StopReason::NoPushAccess,
            format!(
                "no push access to {slug} (permission {}): run `gh repo fork --remote` and retry",
                rv.viewer_permission
            ),
        ));
    };

    git::push(&wt, "origin", &format!("{verified}:refs/heads/{branch}"))
        .map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    ctx.events.info(format!("pushed {} to origin/{branch}", git::short(&verified)));

    let existing = github::find_pr(&target, &head_owner, &branch, &base).map_err(gh_err)?;
    let url = match existing {
        Some(p) if p.head_sha == verified => {
            ctx.events.info(format!("reusing PR #{} for {}", p.number, git::short(&verified)));
            p.url
        }
        other => {
            let issue: github::Issue = ctx.run_dir.read_json("00-issue.json").unwrap_or_default();
            let n = ctx.state.issue_number.unwrap_or(issue.number);
            let read_json = |rel: &str| ctx.run_dir.read_json::<Value>(rel).unwrap_or(Value::Null);
            let body = pr_body(&json!({
                "issue_url": issue.url,
                "issue_number": n,
                "issue_title": issue.title,
                "repro": read_json("20-repro/repro.json"),
                "rca": read_json("30-rca/rca.json"),
                "scope": read_json("40-fix/scope.json"),
                "verify": read_json("50-verify/result.json"),
                "disclosure": format!(
                    "Prepared with LYRA using {}. A human owner reviews this draft before it leaves draft state.",
                    ctx.agent.describe()
                ),
                "branch": branch,
                "verified_commit": verified,
            }))
            .map_err(|e| StageError::from_anyhow(ErrorKind::Config, e))?;
            let title = pr_title(n, &issue.title);
            let io = |e| StageError::from_anyhow(ErrorKind::Io, e);
            ctx.events.artifact(ctx.run_dir.write("60-pr/body.md", &body).map_err(io)?);
            ctx.events.artifact(ctx.run_dir.write("60-pr/title.txt", &title).map_err(io)?);
            let body_path = ctx.run_dir.artifacts().join("60-pr/body.md");
            github::create_draft_pr(&target, &head_ref, &base, &title, &body_path).map_err(|e| {
                let mut err = gh_err(e);
                if let Some(p) = other {
                    err.message =
                        format!("{} (an open PR {} exists with head {})", err.message, p.url, git::short(&p.head_sha));
                }
                err
            })?
        }
    };
    ctx.events
        .artifact(ctx.run_dir.write("60-pr/url.txt", &url).map_err(|e| StageError::from_anyhow(ErrorKind::Io, e))?);
    ctx.events.info(format!("draft PR: {url}"));
    ctx.state.pr_url = Some(url);
    Ok(StageOutcome::Completed)
}

/// `Fix #<n>: <title>`, at most 70 characters.
pub fn pr_title(n: u64, title: &str) -> String {
    let t = format!("Fix #{n}: {}", title.trim());
    if t.chars().count() <= 70 {
        return t;
    }
    let mut s: String = t.chars().take(69).collect::<String>().trim_end().to_string();
    s.push('…');
    s
}

/// Renders `PR_BODY_TEMPLATE` (same minijinja setup as `StageCtx::render`).
pub fn pr_body(values: &Value) -> anyhow::Result<String> {
    let mut env = minijinja::Environment::new();
    env.add_template("pr_body.md", assets::PR_BODY_TEMPLATE)?;
    Ok(env.get_template("pr_body.md")?.render(values)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_is_truncated() {
        assert_eq!(pr_title(7, "Short"), "Fix #7: Short");
        let long = "x".repeat(100);
        let t = pr_title(12345, &long);
        assert_eq!(t.chars().count(), 70);
        assert!(t.starts_with("Fix #12345: xxx"));
        assert!(t.ends_with('…'));
        let exact = format!("Fix #1: {}", "y".repeat(62));
        assert_eq!(pr_title(1, &"y".repeat(62)), exact);
    }

    fn sample() -> Value {
        json!({
            "issue_url": "https://github.com/o/r/issues/42",
            "issue_number": 42,
            "issue_title": "Parser panics on empty header",
            "repro": {"reproduced": true, "crate": "parser", "test_file": "parser/tests/issue_42.rs", "test_name": "empty_header",
                      "command": "cargo test -p parser empty_header -- --exact", "expected": "called `Option::unwrap()` on a `None` value",
                      "observed": "panicked", "registration_file": null, "notes": "", "frozen_commit": "aaa"},
            "rca": {"result": "supported", "culprits": [{"file": "parser/src/lib.rs", "symbol": "parse_header", "lines": [10, 20]}],
                    "mechanism": "unwrap on the first line", "evidence": ["lib.rs:12 unwrap()"], "fix_sketch": "", "confidence": 0.9},
            "scope": {"files": [{"path": "parser/src/lib.rs", "added": 3, "removed": 1}], "total_added": 3, "total_removed": 1,
                      "test_files": [], "warnings": ["touches public API"]},
            "verify": {"ok": true, "steps": [{"name": "build", "command": "cargo build --workspace", "ok": true, "exit_code": 0, "duration_ms": 1200, "log": "50-verify/build.txt"},
                                             {"name": "fmt", "command": "cargo fmt --all -- --check", "ok": true, "exit_code": 0, "duration_ms": 300, "log": "50-verify/fmt.txt"}]},
            "disclosure": "Prepared with LYRA using Claude Code (opus). A human owner reviews this draft before it leaves draft state.",
            "branch": "lyra/issue-42-r1",
            "verified_commit": "0123456789abcdef"
        })
    }

    #[test]
    fn body_renders_all_sections() {
        let b = pr_body(&sample()).unwrap();
        for s in ["## Issue", "## Reproduction", "## Root cause", "## Change", "## Verification", "## Disclosure"] {
            assert!(b.contains(s), "missing {s}:\n{b}");
        }
        assert!(b.contains("Fixes #42"));
        assert!(b.contains("https://github.com/o/r/issues/42"));
        assert!(b.contains("cargo test -p parser empty_header -- --exact"));
        assert!(b.contains("parser/tests/issue_42.rs"));
        assert!(b.contains("unwrap on the first line"));
        assert!(b.contains("`parser/src/lib.rs` `parse_header` (lines 10-20)"));
        assert!(b.contains("| `parser/src/lib.rs` | +3 | -1 |"));
        assert!(b.contains("touches public API"));
        assert!(b.contains("| build | `cargo build --workspace` | pass | 1.2 s |"));
        assert!(b.contains("Prepared with LYRA using Claude Code (opus)"));
        assert!(b.contains("`0123456789abcdef`"));
        assert!(!b.contains("&#x60;") && !b.contains("&quot;"), "no HTML escaping");
    }

    #[test]
    fn body_tolerates_missing_artifacts() {
        let mut v = sample();
        for k in ["repro", "rca", "scope", "verify"] {
            v[k] = Value::Null;
        }
        v["issue_url"] = json!("");
        let b = pr_body(&v).unwrap();
        assert!(b.contains("## Verification"));
        assert!(b.contains("Fixes #42"));
        assert!(b.contains("not recorded"));
    }
}
