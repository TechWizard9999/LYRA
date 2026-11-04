//! S0 Intake: validate the repository, fetch the issue, scan for contribution / AI policy
//! text, and create the run's worktree on a fresh branch from the default branch.

use super::{Stage, StageCtx};
use crate::error::{ErrorKind, StageError, StageOutcome};
use crate::events::StageId;
use crate::{git, github};
use async_trait::async_trait;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub struct IntakeStage;

#[async_trait]
impl Stage for IntakeStage {
    fn id(&self) -> StageId {
        StageId::Intake
    }
    async fn run(&self, ctx: &mut StageCtx) -> StageOutcome {
        match intake(ctx) {
            Ok(()) => StageOutcome::Completed,
            Err(e) => StageOutcome::failed(e),
        }
    }
}

fn io_err(e: anyhow::Error) -> StageError {
    StageError::from_anyhow(ErrorKind::Io, e)
}

fn intake(ctx: &mut StageCtx) -> Result<(), StageError> {
    let repo = ctx.state.request.repo.clone();
    if !repo.is_dir() {
        return Err(StageError::fatal(ErrorKind::Config, format!("repository path {} does not exist", repo.display())));
    }
    if !git::is_repo(&repo) {
        return Err(StageError::fatal(ErrorKind::Git, format!("{} is not a git repository", repo.display())));
    }
    let issue_text = ctx.state.request.issue_text.clone().filter(|t| !t.trim().is_empty());
    let origin_slug = git::remote_url(&repo, "origin").ok().and_then(|u| git::parse_github_slug(&u));
    if origin_slug.is_none() && issue_text.is_none() {
        return Err(StageError::fatal(
            ErrorKind::Git,
            "remote 'origin' is not a GitHub repository; paste the issue text to run offline",
        ));
    }

    // The issue.
    let request_issue = ctx.state.request.issue.clone();
    let issue_slug;
    let issue = if let Some(text) = &issue_text {
        let reference = github::parse_issue_ref(&request_issue, Some(origin_slug.as_deref().unwrap_or("local/local")))
            .map_err(|e| StageError::fatal(ErrorKind::Config, format!("{e:#}")))?;
        issue_slug = Some(reference.slug());
        github::issue_from_text(reference.number, text)
    } else {
        let origin = origin_slug.as_deref().unwrap_or_default();
        let explicit =
            request_issue.contains("github.com/") || request_issue.split('#').next().is_some_and(|s| s.contains('/'));
        // `#n` on a fork refers to the upstream issue tracker.
        let default_slug = if explicit {
            origin.to_string()
        } else {
            github::repo_view(origin)
                .ok()
                .and_then(|rv| rv.parent.filter(|_| rv.is_fork))
                .unwrap_or_else(|| origin.to_string())
        };
        let r = github::parse_issue_ref(&request_issue, Some(&default_slug))
            .map_err(|e| StageError::fatal(ErrorKind::Config, format!("{e:#}")))?;
        issue_slug = Some(r.slug());
        github::issue_view(&r.slug(), r.number).map_err(|e| StageError::from_anyhow(ErrorKind::GitHub, e))?
    };
    let md = ctx.run_dir.write("00-issue.md", github::issue_markdown(&issue)).map_err(io_err)?;
    ctx.events.artifact(md);
    let js = ctx.run_dir.write_json("00-issue.json", &issue).map_err(io_err)?;
    ctx.events.artifact(js);
    ctx.events.info(format!("issue #{}: {}", issue.number, issue.title));

    // Worktree on a unique branch from the default branch.
    let default = git::default_branch(&repo).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    let base = git::rev_parse(&repo, &format!("origin/{default}"))
        .or_else(|_| git::rev_parse(&repo, &default))
        .map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    let branch = format!("lyra/issue-{}-{}", issue.number, ctx.state.id);
    let wt = ctx.run_dir.worktree();
    if wt.exists() {
        let _ = git::worktree_remove(&repo, &wt);
        let _ = std::fs::remove_dir_all(&wt);
    }
    let _ = git::run(&repo, &["worktree", "prune"]);
    if git::run(&repo, &["rev-parse", "--verify", &format!("refs/heads/{branch}")]).is_ok() {
        // Left over from an earlier intake attempt of this same run.
        let _ = git::run(&repo, &["branch", "-D", &branch]);
    }
    git::worktree_add(&repo, &wt, &branch, &base).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    ctx.events.info(format!("worktree {} on {branch} from {default} @ {}", wt.display(), git::short(&base)));

    // Contribution / AI policy text, as of the base commit.
    let mut policy = String::new();
    for f in policy_files(&wt) {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let ex = policy_excerpts(&text);
        if ex.is_empty() {
            continue;
        }
        let rel = f.strip_prefix(&wt).unwrap_or(&f).to_string_lossy().replace('\\', "/");
        policy.push_str(&format!("## {rel}\n\n{}\n\n", ex.join("\n\n")));
    }
    ctx.state.has_policy = !policy.is_empty();
    if ctx.state.has_policy {
        let p =
            ctx.run_dir.write("policy.md", format!("# Contribution policy excerpts\n\n{policy}")).map_err(io_err)?;
        ctx.events.artifact(p);
        ctx.events.warn("the repository has contribution / AI policy text; read policy.md before the PR is opened");
    }

    ctx.state.base_commit = Some(base);
    ctx.state.branch = Some(branch);
    ctx.state.issue_number = Some(issue.number);
    ctx.state.repo_slug = origin_slug;
    ctx.state.issue_slug = issue_slug;
    Ok(())
}

fn name_starts(p: &Path, prefixes: &[&str]) -> bool {
    let name = p.file_name().map(|n| n.to_string_lossy().to_uppercase()).unwrap_or_default();
    prefixes.iter().any(|pre| name.starts_with(pre))
}

fn files_in(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Policy candidates: `CONTRIBUTING*`, `AI_POLICY*`, `CODE_OF_CONDUCT*` at the root,
/// `.github/PULL_REQUEST_TEMPLATE*`, `.github/CONTRIBUTING*`, and `docs/**/code-provenance*`
/// (two levels below `docs/`).
pub fn policy_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = files_in(root)
        .into_iter()
        .filter(|p| name_starts(p, &["CONTRIBUTING", "AI_POLICY", "CODE_OF_CONDUCT"]))
        .collect();
    out.extend(
        files_in(&root.join(".github"))
            .into_iter()
            .filter(|p| name_starts(p, &["PULL_REQUEST_TEMPLATE", "CONTRIBUTING"])),
    );
    let docs = root.join("docs");
    let mut dirs = vec![docs.clone()];
    if let Ok(rd) = std::fs::read_dir(&docs) {
        let mut subs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        subs.sort();
        dirs.extend(subs);
    }
    for d in dirs {
        out.extend(files_in(&d).into_iter().filter(|p| name_starts(p, &["CODE-PROVENANCE"])));
    }
    out
}

/// Paragraphs (blank-line separated) that mention AI / LLMs / agents.
pub fn policy_excerpts(text: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(AI|LLMs?|language models?|Copilot|ChatGPT|Claude|machine[- ]generated|generated code|agents?)\b",
        )
        .unwrap()
    });
    let text = text.replace("\r\n", "\n");
    text.split("\n\n").map(str::trim).filter(|p| !p.is_empty() && re.is_match(p)).map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpts_match_ai_paragraphs_only() {
        let text = "# Contributing\n\nRun cargo fmt.\n\nWe do not accept AI-generated pull requests\nwithout disclosure.\r\n\r\nPlease use Copilot carefully.\n\nMaintainers and email.\n\nNo LLMs.\n\nThe paint is fresh.";
        let ex = policy_excerpts(text);
        assert_eq!(ex.len(), 3, "{ex:?}");
        assert!(ex[0].starts_with("We do not accept AI-generated"));
        assert_eq!(ex[1], "Please use Copilot carefully.");
        assert_eq!(ex[2], "No LLMs.");
        assert!(policy_excerpts("maintain the chain").is_empty());
        assert_eq!(policy_excerpts("Coding agents must disclose.").len(), 1);
    }

    #[test]
    fn finds_policy_files() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        for f in [
            "CONTRIBUTING.md",
            "AI_POLICY.txt",
            "README.md",
            ".github/PULL_REQUEST_TEMPLATE.md",
            ".github/ci.yml",
            "docs/a/code-provenance.md",
            "docs/a/b/code-provenance.md",
        ] {
            let p = r.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        let got: Vec<String> =
            policy_files(r).iter().map(|p| p.strip_prefix(r).unwrap().to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(
            got,
            vec!["AI_POLICY.txt", "CONTRIBUTING.md", ".github/PULL_REQUEST_TEMPLATE.md", "docs/a/code-provenance.md"]
        );
    }
}
