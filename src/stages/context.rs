//! S1 Context: best-effort collectors (related issues/PRs, git history, cargo metadata,
//! repository instructions), then one read-only agent call that summarises them.

use super::{assets, Stage, StageCtx};
use crate::error::{ErrorKind, StageError, StageOutcome};
use crate::events::StageId;
use crate::proc::{self, CommandSpec};
use crate::providers::{AgentOutput, Mode};
use crate::{git, github};
use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

pub struct ContextStage;

#[async_trait]
impl Stage for ContextStage {
    fn id(&self) -> StageId {
        StageId::Context
    }
    async fn run(&self, ctx: &mut StageCtx) -> StageOutcome {
        match context(ctx).await {
            Ok(()) => StageOutcome::Completed,
            Err(e) => StageOutcome::failed(e),
        }
    }
}

const DIR: &str = "10-context";

async fn context(ctx: &mut StageCtx) -> Result<(), StageError> {
    let wt = ctx.run_dir.worktree();
    let issue_md = ctx.run_dir.read("00-issue.md").unwrap_or_default();
    let issue: github::Issue = ctx.run_dir.read_json("00-issue.json").unwrap_or_default();

    // Related issues / PRs (needs GitHub).
    let mut related = json!({});
    if let (Some(slug), Some(n)) = (ctx.state.issue_slug.clone(), ctx.state.issue_number) {
        if ctx.state.request.issue_text.is_none() {
            let kw = title_keywords(&issue.title);
            let search = if kw.is_empty() {
                Vec::new()
            } else {
                github::search_issues(&slug, &kw.join(" "), 10)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|i| i.number != n)
                    .collect()
            };
            let prs = github::prs_mentioning(&slug, n).unwrap_or_default();
            let (parent, subs) = github::parent_and_sub_issues(&slug, n);
            related =
                json!({ "keywords": kw, "similar_issues": search, "prs": prs, "parent": parent, "sub_issues": subs });
        }
    }
    let related_json = serde_json::to_string_pretty(&related).unwrap_or_default();
    write(ctx, "related.json", &related_json);

    // Git history for what the issue mentions.
    let needles = extract_needles(&issue_md);
    let git_history = git_history(&wt, &needles);
    write(ctx, "git-history.md", &git_history);

    // Workspace layout.
    let repo_json = serde_json::to_string_pretty(&cargo_metadata(&wt, ctx).await).unwrap_or_default();
    write(ctx, "repo.json", &repo_json);

    let instructions = instructions(&wt);
    write(ctx, "instructions.md", &instructions);

    // One read-only agent call.
    let prompt = ctx
        .render(
            "context.md",
            assets::CONTEXT_PROMPT,
            json!({
                "issue": issue_md,
                "related_json": related_json,
                "git_history": cap(&git_history, 20_000),
                "repo_json": repo_json,
                "instructions": instructions,
            }),
        )
        .map_err(|e| StageError::from_anyhow(ErrorKind::Config, e))?;
    let head_before = git::head(&wt).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    let out = run_readonly(ctx, StageId::Context, prompt, assets::CONTEXT_SCHEMA).await?;
    let summary = out.structured.unwrap_or(Value::Null);
    let rel = ctx
        .run_dir
        .write_json(format!("{DIR}/summary.json"), &summary)
        .map_err(|e| StageError::from_anyhow(ErrorKind::Io, e))?;
    ctx.events.artifact(rel);
    readonly_guard(&wt, &head_before)?;
    if !summary.get("candidate_files").is_some_and(Value::is_array) {
        return Err(StageError::new(ErrorKind::Schema, "context summary has no candidate_files array"));
    }
    ctx.events.info(format!(
        "context: {} candidate file(s), crate '{}'",
        summary["candidate_files"].as_array().map(Vec::len).unwrap_or(0),
        summary.get("crate").and_then(Value::as_str).unwrap_or("")
    ));
    Ok(())
}

fn write(ctx: &StageCtx, name: &str, contents: &str) {
    match ctx.run_dir.write(format!("{DIR}/{name}"), contents) {
        Ok(rel) => ctx.events.artifact(rel),
        Err(e) => ctx.events.warn(format!("could not write {DIR}/{name}: {e:#}")),
    }
}

/// Runs one read-only agent call with a schema; provider failures are retryable stage failures.
pub(crate) async fn run_readonly(
    ctx: &StageCtx,
    stage: StageId,
    prompt: String,
    schema: &str,
) -> Result<AgentOutput, StageError> {
    let task = ctx.agent_task(stage, prompt, Mode::ReadOnly, Some(assets::schema(schema)));
    let transcript = task.transcript.clone();
    let out = ctx.agent.run(task, &ctx.events).await.map_err(|e| StageError::from_anyhow(ErrorKind::Provider, e))?;
    let details = transcript.strip_prefix(&ctx.run_dir.root).unwrap_or(&transcript).to_path_buf();
    if out.timed_out {
        return Err(StageError::new(ErrorKind::Timeout, "agent call timed out").with_details(details));
    }
    if !out.ok {
        let why = out.failure.clone().unwrap_or_else(|| "agent call failed".into());
        return Err(StageError::new(ErrorKind::Provider, why).with_details(details));
    }
    if out.structured.is_none() {
        return Err(StageError::new(ErrorKind::Schema, "agent returned no structured output").with_details(details));
    }
    Ok(out)
}

/// Change detection for read-only stages: clean worktree and unchanged HEAD.
pub(crate) fn readonly_guard(wt: &Path, head_before: &str) -> Result<(), StageError> {
    let changes = git::changed_files(wt, head_before).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    let head = git::head(wt).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    if !changes.is_empty() || head != head_before {
        let status = format!("{changes:?}");
        return Err(StageError::new(
            ErrorKind::Guard,
            format!(
                "read-only stage changed the worktree (HEAD {} -> {}):\n{status}",
                git::short(head_before),
                git::short(&head)
            ),
        ));
    }
    Ok(())
}

/// Keeps the first `max` chars (on a char boundary), noting the truncation.
pub(crate) fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (truncated, {} bytes total)", &s[..end], s.len())
}

const STOPWORDS: &[&str] = &[
    "when",
    "with",
    "that",
    "this",
    "from",
    "have",
    "does",
    "doesn't",
    "should",
    "would",
    "could",
    "into",
    "after",
    "before",
    "while",
    "where",
    "which",
    "there",
    "their",
    "about",
    "being",
    "using",
    "used",
    "some",
    "more",
    "than",
    "then",
    "them",
    "they",
    "will",
    "your",
    "what",
    "also",
    "only",
    "just",
    "make",
    "makes",
    "error",
    "issue",
    "panic",
    "panics",
    "fails",
    "failed",
    "wrong",
    "incorrect",
    "returns",
    "instead",
    "works",
    "work",
];

/// Search keywords from an issue title: words longer than 3 chars, lowercase, no stopwords, max 6.
pub fn title_keywords(title: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in title.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let w = w.to_lowercase();
        if w.chars().count() > 3 && !STOPWORDS.contains(&w.as_str()) && !out.contains(&w) {
            out.push(w);
            if out.len() == 6 {
                break;
            }
        }
    }
    out
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Needles {
    pub paths: Vec<String>,
    /// `(path, line)` from `file.rs:123` references.
    pub lines: Vec<(String, u32)>,
    pub symbols: Vec<String>,
    pub errors: Vec<String>,
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    let s = s.trim();
    if !s.is_empty() && v.len() < 10 && !v.iter().any(|x| x == s) {
        v.push(s.to_string());
    }
}

/// Paths (`*.rs`, with optional `:line`), backticked identifiers and error lines from issue text.
pub fn extract_needles(issue_text: &str) -> Needles {
    let path_re = Regex::new(r"([\w./-]+\.rs)(?::(\d+))?").unwrap();
    let sym_re = Regex::new(r"`([A-Za-z_][\w:]*(?:\(\))?)`").unwrap();
    let quoted_re = Regex::new(r#""([^"\n]{13,})""#).unwrap();
    let mut n = Needles::default();
    for c in path_re.captures_iter(issue_text) {
        let p = c[1].trim_start_matches("./").to_string();
        push_unique(&mut n.paths, &p);
        if let Some(line) = c.get(2).and_then(|m| m.as_str().parse::<u32>().ok()) {
            if n.lines.len() < 10 && !n.lines.contains(&(p.clone(), line)) {
                n.lines.push((p, line));
            }
        }
    }
    for c in sym_re.captures_iter(issue_text) {
        let s = c[1].trim_end_matches("()");
        if !s.ends_with(".rs") {
            push_unique(&mut n.symbols, s);
        }
    }
    for line in issue_text.lines() {
        if line.contains("panicked at") || line.contains("error[") || line.contains("thread '") {
            push_unique(&mut n.errors, line);
        }
    }
    for c in quoted_re.captures_iter(issue_text) {
        push_unique(&mut n.errors, &c[1]);
    }
    n
}

fn fenced(s: &str) -> String {
    if s.trim().is_empty() {
        "_(nothing)_\n".into()
    } else {
        format!("```\n{}\n```\n", s.trim_end())
    }
}

fn git_history(wt: &Path, n: &Needles) -> String {
    let mut md = String::from("# Git history\n");
    for p in &n.paths {
        let path = resolve_path(wt, p);
        md.push_str(&format!("\n## `{path}`\n\n### git log --follow\n\n{}", fenced(&git::log_follow(wt, &path, 10))));
        md.push_str(&format!("\n### git shortlog\n\n{}", fenced(&git::shortlog(wt, &path))));
    }
    for s in n.symbols.iter().chain(n.errors.iter()) {
        md.push_str(&format!("\n## git log -S {s:?}\n\n{}", fenced(&git::log_s(wt, s, 10))));
    }
    for (p, line) in &n.lines {
        let path = resolve_path(wt, p);
        let from = line.saturating_sub(10).max(1);
        md.push_str(&format!(
            "\n## git blame `{path}` {from}-{}\n\n{}",
            line + 10,
            fenced(&git::blame(wt, &path, from, line + 10))
        ));
    }
    if n.paths.is_empty() && n.symbols.is_empty() && n.errors.is_empty() {
        md.push_str("\nThe issue mentions no file paths, symbols or error lines.\n");
    }
    md
}

/// Issue paths are often absolute or prefixed (`/home/u/proj/src/x.rs`, `crates/a/src/x.rs`):
/// strip leading components until the path exists in the worktree.
fn resolve_path(wt: &Path, p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    for i in 0..parts.len() {
        let cand = parts[i..].join("/");
        if wt.join(&cand).exists() {
            return cand;
        }
    }
    p.to_string()
}

async fn cargo_metadata(wt: &Path, ctx: &StageCtx) -> Value {
    let spec = CommandSpec::new("cargo", wt).args(["metadata", "--format-version", "1", "--no-deps"]).scrubbed();
    match proc::run(&spec, Duration::from_secs(120), &ctx.cancel).await {
        Ok(out) if out.success() => parse_cargo_metadata(&out.stdout),
        Ok(out) => json!({ "error": cap(&out.combined(), 4000) }),
        Err(e) => json!({ "error": format!("{e:#}") }),
    }
}

/// `{workspace_root, members:[{name, manifest_path, has_tests_dir}]}` from `cargo metadata`.
pub fn parse_cargo_metadata(text: &str) -> Value {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return json!({ "error": "cargo metadata printed invalid JSON" });
    };
    let members: Vec<Value> = v["packages"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|p| {
                    let manifest = p["manifest_path"].as_str().unwrap_or_default();
                    let has_tests = Path::new(manifest).parent().is_some_and(|d| d.join("tests").is_dir());
                    json!({ "name": p["name"], "manifest_path": manifest, "has_tests_dir": has_tests })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({ "workspace_root": v["workspace_root"], "members": members })
}

fn instructions(wt: &Path) -> String {
    let mut md = String::new();
    for f in ["AGENTS.md", "CLAUDE.md", ".github/copilot-instructions.md", "CONTRIBUTING.md"] {
        if let Ok(text) = std::fs::read_to_string(wt.join(f)) {
            md.push_str(&format!("## {f}\n\n{}\n\n", cap(text.trim(), 4000)));
        }
    }
    if md.is_empty() {
        md.push_str("_The repository has no AGENTS.md, CLAUDE.md, copilot-instructions.md or CONTRIBUTING.md._\n");
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_from_title() {
        assert_eq!(title_keywords("Parser panics when the header is empty"), vec!["parser", "header", "empty"]);
        let k = title_keywords("alpha beta gamma delta epsilon zeta theta iota kappa");
        assert_eq!(k.len(), 6);
        assert_eq!(title_keywords("a an the"), Vec::<String>::new());
        assert_eq!(title_keywords("Tokio tokio TOKIO"), vec!["tokio"]);
    }

    #[test]
    fn needles_from_issue() {
        let text = r#"Calling `parse_header()` on an empty input panics.

thread 'main' panicked at src/parser.rs:120:9:
called `Option::unwrap()` on a `None` value

Also see crates/core/src/lib.rs and "expected a non-empty header line". `src/parser.rs` again.
"#;
        let n = extract_needles(text);
        assert_eq!(n.paths, vec!["src/parser.rs", "crates/core/src/lib.rs"]);
        assert_eq!(n.lines, vec![("src/parser.rs".to_string(), 120)]);
        assert_eq!(n.symbols, vec!["parse_header", "Option::unwrap", "None"]);
        assert_eq!(n.errors[0], "thread 'main' panicked at src/parser.rs:120:9:");
        assert!(n.errors.contains(&"expected a non-empty header line".to_string()));
    }

    #[test]
    fn needles_are_capped() {
        let text: String = (0..30).map(|i| format!("f{i}.rs `sym{i}`\n")).collect();
        let n = extract_needles(&text);
        assert_eq!(n.paths.len(), 10);
        assert_eq!(n.symbols.len(), 10);
    }

    #[test]
    fn cargo_metadata_parsing() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/tests")).unwrap();
        let manifest = d.path().join("a/Cargo.toml").to_string_lossy().to_string();
        let text = json!({ "workspace_root": "/w", "packages": [{ "name": "a", "manifest_path": manifest }, { "name": "b", "manifest_path": "/nope/Cargo.toml" }] }).to_string();
        let v = parse_cargo_metadata(&text);
        assert_eq!(v["workspace_root"], "/w");
        assert_eq!(v["members"][0]["has_tests_dir"], true);
        assert_eq!(v["members"][1]["has_tests_dir"], false);
        assert!(parse_cargo_metadata("nope")["error"].is_string());
    }

    #[test]
    fn cap_truncates_on_char_boundary() {
        assert_eq!(cap("abc", 10), "abc");
        let c = cap("ééé", 3);
        assert!(c.starts_with("é\n"));
    }

    #[test]
    fn guard_detects_changes() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        git::run(p, &["init", "-q", "-b", "main"]).unwrap();
        git::run(p, &["config", "user.email", "t@example.com"]).unwrap();
        git::run(p, &["config", "user.name", "T"]).unwrap();
        std::fs::write(p.join("a.txt"), "a").unwrap();
        let head = git::commit_all(p, "init").unwrap();
        assert!(readonly_guard(p, &head).is_ok());
        std::fs::write(p.join("b.txt"), "b").unwrap();
        assert_eq!(readonly_guard(p, &head).unwrap_err().kind, ErrorKind::Guard);
        git::reset_hard_clean(p, &head).unwrap();
        std::fs::write(p.join(".git/info/exclude"), ".cargo/\n").unwrap();
        std::fs::create_dir(p.join(".cargo")).unwrap();
        std::fs::write(p.join(".cargo/config.toml"), "[alias]\n").unwrap();
        assert!(git::is_clean(p).unwrap(), "the status-only check misses ignored files");
        assert_eq!(readonly_guard(p, &head).unwrap_err().kind, ErrorKind::Guard);
    }

    #[test]
    fn history_resolves_prefixed_paths() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src/x.rs"), "").unwrap();
        assert_eq!(resolve_path(d.path(), "/home/u/proj/src/x.rs"), "src/x.rs");
        assert_eq!(resolve_path(d.path(), "src/missing.rs"), "src/missing.rs");
    }
}
