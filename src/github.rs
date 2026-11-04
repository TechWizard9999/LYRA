//! Thin, synchronous wrappers over the `gh` CLI. JSON parsing lives in pure functions that
//! take `&str` so it can be tested without calling `gh`.

use crate::proc::CommandSpec;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// Runs `gh <args>` and returns trimmed stdout; non-zero exit is an error with stderr.
fn gh(args: &[&str]) -> Result<String> {
    crate::git::command(
        &CommandSpec::new("gh", std::env::current_dir()?).args(args.iter().copied()).env("GH_PROMPT_DISABLED", "1"),
    )
}

// ---------------------------------------------------------------------------------------------
// Issue references

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl IssueRef {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

fn split_slug(slug: &str) -> Result<(String, String)> {
    let mut parts = slug.trim().split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(o), Some(r), None) if !o.is_empty() && !r.is_empty() => Ok((o.to_string(), r.to_string())),
        _ => bail!("not an owner/repo slug: '{slug}'"),
    }
}

fn parse_number(s: &str) -> Result<u64> {
    let n = s.trim().parse::<u64>().with_context(|| format!("not an issue number: '{s}'"))?;
    if n == 0 {
        bail!("issue number must be positive");
    }
    Ok(n)
}

/// Accepts `https://github.com/o/r/issues/123`, `o/r#123`, `#123` and `123` (the last two need
/// `default_slug`).
pub fn parse_issue_ref(input: &str, default_slug: Option<&str>) -> Result<IssueRef> {
    let s = input.trim();
    if let Some(rest) = s.strip_prefix("https://github.com/") {
        let rest = rest.split(['?', '#']).next().unwrap_or(rest);
        let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
        if parts.len() == 4 && parts[2] == "issues" && !parts[0].is_empty() && !parts[1].is_empty() {
            return Ok(IssueRef { owner: parts[0].into(), repo: parts[1].into(), number: parse_number(parts[3])? });
        }
        bail!("not a GitHub issue URL: '{input}'");
    }
    if let Some((slug, n)) = s.split_once('#') {
        if !slug.is_empty() {
            let (owner, repo) = split_slug(slug)?;
            return Ok(IssueRef { owner, repo, number: parse_number(n)? });
        }
    }
    let number = parse_number(s.trim_start_matches('#'))?;
    let slug = default_slug.with_context(|| format!("'{input}' needs a repository (owner/repo#n or an issue URL)"))?;
    let (owner, repo) = split_slug(slug)?;
    Ok(IssueRef { owner, repo, number })
}

// ---------------------------------------------------------------------------------------------
// Issues

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Comment {
    pub author: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub state: String,
    pub author: String,
    pub labels: Vec<String>,
    pub comments: Vec<Comment>,
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn login(v: &Value, key: &str) -> String {
    v.get(key).and_then(|a| a.get("login")).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// Parses `gh issue view --json number,title,body,url,state,author,labels,comments`.
pub fn parse_issue_json(text: &str) -> Result<Issue> {
    let v: Value = serde_json::from_str(text).context("parsing gh issue JSON")?;
    let labels = v
        .get("labels")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|l| l.get("name").and_then(Value::as_str).map(String::from)).collect())
        .unwrap_or_default();
    let comments = v
        .get("comments")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|c| Comment { author: login(c, "author"), body: s(c, "body"), created_at: s(c, "createdAt") })
                .collect()
        })
        .unwrap_or_default();
    Ok(Issue {
        number: v.get("number").and_then(Value::as_u64).context("issue JSON has no number")?,
        title: s(&v, "title"),
        body: s(&v, "body"),
        url: s(&v, "url"),
        state: s(&v, "state"),
        author: login(&v, "author"),
        labels,
        comments,
    })
}

pub fn issue_view(slug: &str, n: u64) -> Result<Issue> {
    let out = gh(&[
        "issue",
        "view",
        &n.to_string(),
        "--repo",
        slug,
        "--json",
        "number,title,body,url,state,author,labels,comments",
    ])?;
    parse_issue_json(&out)
}

/// An issue from pasted text: title = first non-empty line, body = the rest.
pub fn issue_from_text(number: u64, text: &str) -> Issue {
    let mut lines = text.lines();
    let mut title = String::new();
    for l in lines.by_ref() {
        if !l.trim().is_empty() {
            title = l.trim().trim_start_matches('#').trim().to_string();
            break;
        }
    }
    let body = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    Issue { number, title, body, state: "OPEN".into(), ..Default::default() }
}

pub fn issue_markdown(issue: &Issue) -> String {
    let mut md = format!("# #{} {}\n\n", issue.number, issue.title);
    let mut meta = Vec::new();
    if !issue.url.is_empty() {
        meta.push(issue.url.clone());
    }
    if !issue.author.is_empty() {
        meta.push(format!("by @{}", issue.author));
    }
    if !issue.state.is_empty() {
        meta.push(issue.state.to_lowercase());
    }
    if !issue.labels.is_empty() {
        meta.push(format!("labels: {}", issue.labels.join(", ")));
    }
    if !meta.is_empty() {
        md.push_str(&meta.join(" · "));
        md.push_str("\n\n");
    }
    md.push_str(issue.body.trim());
    md.push('\n');
    if !issue.comments.is_empty() {
        md.push_str("\n## Comments\n");
        for c in &issue.comments {
            md.push_str(&format!("\n### @{} ({})\n\n{}\n", c.author, c.created_at, c.body.trim()));
        }
    }
    md
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueSummary {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
}

/// Parses a `gh ... list --json number,title,state,url` array.
pub fn parse_summaries_json(text: &str) -> Result<Vec<IssueSummary>> {
    let v: Value = serde_json::from_str(text).context("parsing gh list JSON")?;
    Ok(v.as_array().map(|a| a.iter().filter_map(summary_of).collect()).unwrap_or_default())
}

/// Works for both `gh` JSON (`url`) and REST API objects (`html_url`).
fn summary_of(v: &Value) -> Option<IssueSummary> {
    // REST objects carry an API `url` plus the browser `html_url`; prefer the latter.
    let url = v.get("html_url").or_else(|| v.get("url")).and_then(Value::as_str).unwrap_or_default();
    Some(IssueSummary {
        number: v.get("number")?.as_u64()?,
        title: s(v, "title"),
        state: s(v, "state"),
        url: url.to_string(),
    })
}

pub fn search_issues(slug: &str, query: &str, limit: usize) -> Result<Vec<IssueSummary>> {
    let out = gh(&[
        "issue",
        "list",
        "--repo",
        slug,
        "--search",
        query,
        "--state",
        "all",
        "--limit",
        &limit.to_string(),
        "--json",
        "number,title,state,url",
    ])?;
    parse_summaries_json(&out)
}

pub fn prs_mentioning(slug: &str, n: u64) -> Result<Vec<IssueSummary>> {
    let out = gh(&[
        "pr",
        "list",
        "--repo",
        slug,
        "--search",
        &n.to_string(),
        "--state",
        "all",
        "--limit",
        "10",
        "--json",
        "number,title,state,url",
    ])?;
    parse_summaries_json(&out)
}

/// Parent issue and sub-issues; best effort (errors give empty lists).
pub fn parent_and_sub_issues(slug: &str, n: u64) -> (Vec<IssueSummary>, Vec<IssueSummary>) {
    let parent = gh(&["api", &format!("repos/{slug}/issues/{n}/parent")])
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| summary_of(&v))
        .into_iter()
        .collect();
    let subs = gh(&["api", &format!("repos/{slug}/issues/{n}/sub_issues")])
        .ok()
        .and_then(|t| parse_summaries_json(&t).ok())
        .unwrap_or_default();
    (parent, subs)
}

// ---------------------------------------------------------------------------------------------
// Repositories and PRs

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoView {
    pub default_branch: String,
    pub is_fork: bool,
    /// `owner/repo` of the upstream when this is a fork.
    pub parent: Option<String>,
    pub viewer_permission: String,
}

/// Parses `gh repo view --json defaultBranchRef,isFork,parent,viewerPermission`.
pub fn parse_repo_view(text: &str) -> Result<RepoView> {
    let v: Value = serde_json::from_str(text).context("parsing gh repo view JSON")?;
    let parent = v.get("parent").filter(|p| !p.is_null()).and_then(|p| {
        if let Some(nwo) = p.get("nameWithOwner").and_then(Value::as_str) {
            return Some(nwo.to_string());
        }
        let owner = login(p, "owner");
        let name = s(p, "name");
        (!owner.is_empty() && !name.is_empty()).then(|| format!("{owner}/{name}"))
    });
    Ok(RepoView {
        default_branch: v.get("defaultBranchRef").map(|b| s(b, "name")).unwrap_or_default(),
        is_fork: v.get("isFork").and_then(Value::as_bool).unwrap_or(false),
        parent,
        viewer_permission: s(&v, "viewerPermission"),
    })
}

pub fn repo_view(slug: &str) -> Result<RepoView> {
    parse_repo_view(&gh(&["repo", "view", slug, "--json", "defaultBranchRef,isFork,parent,viewerPermission"])?)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrInfo {
    pub number: u64,
    pub url: String,
    pub head_sha: String,
    pub head_owner: String,
    #[serde(default)]
    pub is_draft: bool,
}

/// Parses `gh pr list --json number,url,headRefOid,headRepositoryOwner` and keeps PRs whose
/// head repository owner is `head_owner` (case-insensitive).
pub fn parse_pr_list(text: &str, head_owner: &str) -> Result<Vec<PrInfo>> {
    let v: Value = serde_json::from_str(text).context("parsing gh pr list JSON")?;
    Ok(v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    Some(PrInfo {
                        number: p.get("number")?.as_u64()?,
                        url: s(p, "url"),
                        head_sha: s(p, "headRefOid"),
                        head_owner: login(p, "headRepositoryOwner"),
                        is_draft: p.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .filter(|p| p.head_owner.eq_ignore_ascii_case(head_owner))
                .collect()
        })
        .unwrap_or_default())
}

pub fn find_pr(target_slug: &str, head_owner: &str, branch: &str, base: &str) -> Result<Option<PrInfo>> {
    let out = gh(&[
        "pr",
        "list",
        "--repo",
        target_slug,
        "--head",
        branch,
        "--base",
        base,
        "--state",
        "open",
        "--json",
        "number,url,headRefOid,headRepositoryOwner,isDraft",
    ])?;
    Ok(parse_pr_list(&out, head_owner)?.into_iter().next())
}

/// Before a push, inspect every open PR for this source branch, including PRs
/// a human retargeted away from the repository's current default branch.
pub fn find_prs_for_branch(target_slug: &str, head_owner: &str, branch: &str) -> Result<Vec<PrInfo>> {
    let out = gh(&[
        "pr",
        "list",
        "--repo",
        target_slug,
        "--head",
        branch,
        "--state",
        "open",
        "--limit",
        "1000",
        "--json",
        "number,url,headRefOid,headRepositoryOwner,isDraft",
    ])?;
    parse_pr_list(&out, head_owner)
}

/// `head` is `owner:branch` (fork) or `branch`. Returns the PR URL (last stdout line).
pub fn create_draft_pr(
    target_slug: &str,
    source_slug: &str,
    head: &str,
    base: &str,
    title: &str,
    body_path: &Path,
) -> Result<String> {
    // REST supports organization-owned forks; `gh pr create --head org:branch` does not.
    let payload = draft_payload(source_slug, head, base, title, &std::fs::read_to_string(body_path)?)?;
    let spec = CommandSpec::new("gh", std::env::current_dir()?)
        .args(["api", "--method", "POST", &format!("repos/{target_slug}/pulls"), "--input", "-"])
        .stdin(serde_json::to_string(&payload)?)
        .env("GH_PROMPT_DISABLED", "1");
    let out = crate::git::command(&spec)?;
    let value: Value = serde_json::from_str(&out)?;
    value
        .get("html_url")
        .and_then(Value::as_str)
        .filter(|u| u.starts_with("https://github.com/"))
        .map(String::from)
        .context("create pull request returned no GitHub URL")
}

fn draft_payload(source_slug: &str, head: &str, base: &str, title: &str, body: &str) -> Result<Value> {
    let (_, repo) = split_slug(source_slug)?;
    Ok(
        serde_json::json!({ "title": title, "head": head, "head_repo": repo, "base": base, "body": body, "draft": true }),
    )
}

pub fn gh_user() -> Result<String> {
    let u = gh(&["api", "user", "--jq", ".login"])?;
    if u.trim().is_empty() {
        bail!("gh api user returned no login");
    }
    Ok(u.trim().to_string())
}

pub fn auth_ok() -> bool {
    gh(&["auth", "status"]).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_ref_variants() {
        let r = parse_issue_ref("https://github.com/o/r/issues/123", None).unwrap();
        assert_eq!((r.owner.as_str(), r.repo.as_str(), r.number), ("o", "r", 123));
        let r = parse_issue_ref("https://github.com/o/r/issues/7#issuecomment-1", None).unwrap();
        assert_eq!(r.number, 7);
        let r = parse_issue_ref("o/r#45", None).unwrap();
        assert_eq!(r.slug(), "o/r");
        assert_eq!(r.number, 45);
        let r = parse_issue_ref("#9", Some("a/b")).unwrap();
        assert_eq!((r.slug(), r.number), ("a/b".to_string(), 9));
        let r = parse_issue_ref(" 10 ", Some("a/b")).unwrap();
        assert_eq!(r.number, 10);
        assert!(parse_issue_ref("#9", None).is_err());
        assert!(parse_issue_ref("abc", Some("a/b")).is_err());
        assert!(parse_issue_ref("https://github.com/o/r", None).is_err());
        assert!(parse_issue_ref("https://github.com/o/r/pull/123", None).is_err());
        assert!(parse_issue_ref("https://fakegithub.com/o/r/issues/123", None).is_err());
    }

    const ISSUE: &str = r#"{"author":{"id":"x","is_bot":false,"login":"alice","name":"Alice"},
      "body":"It panics.\n\nSteps...","comments":[{"id":"c1","author":{"login":"bob"},"authorAssociation":"NONE",
      "body":"Same here","createdAt":"2026-01-02T03:04:05Z","includesCreatedEdit":false,"reactionGroups":[],"url":"u"}],
      "labels":[{"id":"l","name":"bug","description":"","color":"red"},{"id":"m","name":"P1","description":"","color":"x"}],
      "number":42,"state":"OPEN","title":"Parser panics on empty header","url":"https://github.com/o/r/issues/42"}"#;

    #[test]
    fn parses_issue_json() {
        let i = parse_issue_json(ISSUE).unwrap();
        assert_eq!(i.number, 42);
        assert_eq!(i.author, "alice");
        assert_eq!(i.labels, vec!["bug", "P1"]);
        assert_eq!(i.comments.len(), 1);
        assert_eq!(i.comments[0].author, "bob");
        assert_eq!(i.comments[0].created_at, "2026-01-02T03:04:05Z");
        assert!(parse_issue_json("{}").is_err());
    }

    #[test]
    fn issue_markdown_has_sections() {
        let md = issue_markdown(&parse_issue_json(ISSUE).unwrap());
        assert!(md.starts_with("# #42 Parser panics on empty header\n"));
        assert!(md.contains("https://github.com/o/r/issues/42 · by @alice · open · labels: bug, P1"));
        assert!(md.contains("It panics."));
        assert!(md.contains("## Comments"));
        assert!(md.contains("### @bob (2026-01-02T03:04:05Z)\n\nSame here"));
        let plain = issue_markdown(&issue_from_text(1, "t\nb"));
        assert!(!plain.contains("## Comments"));
    }

    #[test]
    fn issue_from_pasted_text() {
        let i = issue_from_text(5, "\n\n# Title here\n\nbody line 1\nbody line 2\n");
        assert_eq!(i.number, 5);
        assert_eq!(i.title, "Title here");
        assert_eq!(i.body, "body line 1\nbody line 2");
    }

    #[test]
    fn parses_summaries() {
        let v = parse_summaries_json(r#"[{"number":1,"title":"a","state":"OPEN","url":"u1"},{"number":2,"title":"b","state":"closed","html_url":"h2"}]"#).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].url, "h2");
        assert!(parse_summaries_json("[]").unwrap().is_empty());
    }

    #[test]
    fn parses_repo_view() {
        let rv = parse_repo_view(r#"{"defaultBranchRef":{"name":"main"},"isFork":true,"parent":{"id":"x","name":"up","owner":{"id":"y","login":"org"}},"viewerPermission":"ADMIN"}"#).unwrap();
        assert_eq!(rv.default_branch, "main");
        assert!(rv.is_fork);
        assert_eq!(rv.parent.as_deref(), Some("org/up"));
        assert_eq!(rv.viewer_permission, "ADMIN");
        let rv = parse_repo_view(
            r#"{"defaultBranchRef":{"name":"master"},"isFork":false,"parent":null,"viewerPermission":"READ"}"#,
        )
        .unwrap();
        assert!(!rv.is_fork);
        assert!(rv.parent.is_none());
    }

    #[test]
    fn pr_list_filters_by_owner() {
        let text = r#"[{"number":3,"url":"u3","headRefOid":"aaa","headRepositoryOwner":{"id":"1","login":"someone"}},
                       {"number":4,"url":"u4","headRefOid":"bbb","headRepositoryOwner":{"id":"2","login":"Me"}}]"#;
        let v = parse_pr_list(text, "me").unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].number, 4);
        assert_eq!(v[0].head_sha, "bbb");
        assert!(parse_pr_list(text, "nobody").unwrap().is_empty());
    }

    #[test]
    fn draft_payload_supports_organization_fork() {
        let v = draft_payload("org/fork", "org:fix", "main", "Fix", "Body\nline").unwrap();
        assert_eq!(v["head"], "org:fix");
        assert_eq!(v["head_repo"], "fork");
        assert_eq!(v["draft"], true);
        assert_eq!(v["body"], "Body\nline");
    }
}
