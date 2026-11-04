//! Thin, synchronous wrappers over the `git` CLI. Every function takes the directory to run in.

use crate::proc::{self, CommandSpec};
use anyhow::{bail, Context, Result};
use std::cell::RefCell;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

thread_local! {
    static RUN_CONTEXT: RefCell<(Duration, CancellationToken)> = RefCell::new((Duration::from_secs(1800), CancellationToken::new()));
}

pub fn with_run_context<T>(timeout: Duration, cancel: CancellationToken, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<(Duration, CancellationToken)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(old) = self.0.take() {
                RUN_CONTEXT.with(|ctx| *ctx.borrow_mut() = old);
            }
        }
    }
    let old = RUN_CONTEXT.with(|ctx| std::mem::replace(&mut *ctx.borrow_mut(), (timeout, cancel)));
    let _restore = Restore(Some(old));
    f()
}

pub(crate) fn command(spec: &CommandSpec) -> Result<String> {
    let (timeout, cancel) = RUN_CONTEXT.with(|ctx| ctx.borrow().clone());
    let out = proc::run_blocking(spec, timeout, &cancel)?;
    if !out.success() {
        bail!(
            "{} failed (exit {:?}, timeout={}, cancelled={}): {}",
            spec.display(),
            out.exit_code,
            out.timed_out,
            out.cancelled,
            out.combined()
        );
    }
    Ok(out.stdout.trim_end_matches(['\r', '\n']).to_owned())
}

/// Runs `git <args>` in `dir` and returns trimmed stdout; non-zero exit is an error with stderr.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let hooks = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let spec = CommandSpec::new("git", dir)
        .args(["-c", &format!("core.hooksPath={hooks}"), "-c", "core.longpaths=true", "-c", "core.fsmonitor=false"])
        .args(args.iter().copied())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
    command(&spec).with_context(|| format!("running git in {}", dir.display()))
}

/// Like `run` but returns stdout even on failure (for best-effort collectors).
pub fn run_lenient(dir: &Path, args: &[&str]) -> String {
    run(dir, args).unwrap_or_default()
}

pub fn is_repo(dir: &Path) -> bool {
    run(dir, &["rev-parse", "--is-inside-work-tree"]).map(|s| s == "true").unwrap_or(false)
}

pub fn head(dir: &Path) -> Result<String> {
    run(dir, &["rev-parse", "HEAD"])
}

pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

pub fn rev_parse(dir: &Path, rev: &str) -> Result<String> {
    run(dir, &["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
}

pub fn status_porcelain(dir: &Path) -> Result<String> {
    run(dir, &["status", "--porcelain", "--untracked-files=all"])
}

pub fn is_clean(dir: &Path) -> Result<bool> {
    Ok(status_porcelain(dir)?.trim().is_empty())
}

/// `git reset --hard <commit> && git clean -fd` — restores a checkpoint, removing files the
/// previous attempt created.
pub fn reset_hard_clean(dir: &Path, commit: &str) -> Result<()> {
    run(dir, &["reset", "--hard", commit])?;
    run(dir, &["clean", "-fdx"])?;
    Ok(())
}

/// Name of the default branch (`main`/`master`), from origin/HEAD when available.
pub fn default_branch(repo: &Path) -> Result<String> {
    if let Ok(s) = run(repo, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        if let Some(b) = s.strip_prefix("origin/") {
            return Ok(b.to_string());
        }
    }
    for cand in ["main", "master"] {
        if run(repo, &["rev-parse", "--verify", &format!("refs/heads/{cand}")]).is_ok() {
            return Ok(cand.to_string());
        }
    }
    run(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// Creates a worktree at `path` on a new branch `branch` starting from `base_ref`.
pub fn worktree_add(repo: &Path, path: &Path, branch: &str, base_ref: &str) -> Result<()> {
    let p = path.to_string_lossy().to_string();
    run(repo, &["worktree", "add", "-b", branch, &p, base_ref])?;
    Ok(())
}

pub fn worktree_remove(repo: &Path, path: &Path) -> Result<()> {
    let p = path.to_string_lossy().to_string();
    run(repo, &["worktree", "remove", "--force", &p])?;
    Ok(())
}

/// A fresh detached checkout makes verification independent of ignored files in the agent workspace.
pub fn verification_worktree(repo: &Path, path: &Path, commit: &str) -> Result<()> {
    run(repo, &["worktree", "add", "--detach", &path.to_string_lossy(), commit])?;
    Ok(())
}

pub fn current_branch(dir: &Path) -> Result<String> {
    run(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// Stages everything and commits with the configured user identity. Returns the new HEAD.
pub fn commit_all(dir: &Path, message: &str) -> Result<String> {
    run(dir, &["add", "-A"])?;
    run(dir, &["commit", "-q", "-m", message])?;
    head(dir)
}

/// `git diff <from>..<to>` (or working tree vs HEAD when `to` is None).
pub fn diff(dir: &Path, from: &str, to: Option<&str>) -> Result<String> {
    match to {
        Some(t) => run(dir, &["diff", "--no-color", "--no-ext-diff", "--no-textconv", from, t]),
        None => run(dir, &["diff", "--no-color", "--no-ext-diff", "--no-textconv", from]),
    }
}

/// Working tree + index vs `commit`, including untracked files, as `(status, path)` pairs.
/// Status is one of A/M/D/R/??.
pub fn changed_files(dir: &Path, commit: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let tracked = run(dir, &["diff", "--name-status", "--no-renames", "-z", commit])?;
    out.extend(parse_name_status(&tracked));
    // Deliberately include ignored files: an ignored .cargo/config.toml can change test behavior.
    let untracked = run(dir, &["ls-files", "--others", "-z"])?;
    for p in untracked.split('\0').filter(|l| !l.is_empty()) {
        out.push(("??".to_string(), p.replace('\\', "/")));
    }
    Ok(out)
}

/// Files changed between two commits as `(status, path)`.
pub fn changed_files_between(dir: &Path, from: &str, to: &str) -> Result<Vec<(String, String)>> {
    let text = run(dir, &["diff", "--name-status", "--no-renames", "-z", from, to])?;
    Ok(parse_name_status(&text))
}

fn parse_name_status(text: &str) -> Vec<(String, String)> {
    text.split('\0')
        .collect::<Vec<_>>()
        .as_chunks::<2>()
        .0
        .iter()
        .filter_map(|pair| Some((pair[0].chars().next()?.to_string(), pair[1].replace('\\', "/"))))
        .collect()
}

/// Contents of `path` at `commit`.
pub fn show_file(dir: &Path, commit: &str, path: &str) -> Result<String> {
    run(dir, &["show", &format!("{commit}:{path}")])
}

pub fn log_s(dir: &Path, needle: &str, n: usize) -> String {
    run_lenient(dir, &["log", "--oneline", "-n", &n.to_string(), "-S", needle])
}

pub fn log_follow(dir: &Path, path: &str, n: usize) -> String {
    run_lenient(dir, &["log", "--oneline", "-n", &n.to_string(), "--follow", "--", path])
}

pub fn blame(dir: &Path, path: &str, from: u32, to: u32) -> String {
    run_lenient(dir, &["blame", "-L", &format!("{from},{to}"), "--date=short", "--", path])
}

pub fn shortlog(dir: &Path, path: &str) -> String {
    run_lenient(dir, &["shortlog", "-sn", "HEAD", "--", path])
}

pub fn remote_url(dir: &Path, remote: &str) -> Result<String> {
    run(dir, &["remote", "get-url", remote])
}

pub fn push(dir: &Path, remote: &str, refspec: &str) -> Result<()> {
    run(dir, &["push", "-u", remote, refspec])?;
    Ok(())
}

/// `owner/repo` from an https or ssh GitHub remote URL.
pub fn parse_github_slug(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let rest = if let Some(r) = url.strip_prefix("git@github.com:") {
        r
    } else {
        url.strip_prefix("https://github.com/").or_else(|| url.strip_prefix("ssh://git@github.com/"))?
    };
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q", "-b", "main"]).unwrap();
        run(dir.path(), &["config", "user.email", "t@example.com"]).unwrap();
        run(dir.path(), &["config", "user.name", "T"]).unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        commit_all(dir.path(), "init").unwrap();
        dir
    }

    #[test]
    fn slug_parsing() {
        assert_eq!(parse_github_slug("https://github.com/org/repo.git").unwrap(), "org/repo");
        assert_eq!(parse_github_slug("git@github.com:org/repo.git").unwrap(), "org/repo");
        assert_eq!(parse_github_slug("https://github.com/org/repo/").unwrap(), "org/repo");
        assert!(parse_github_slug("https://gitlab.com/org/repo").is_none());
    }

    #[test]
    fn worktree_commit_changes_and_reset() {
        let repo = init_repo();
        let base = head(repo.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt = wt_dir.path().join("wt");
        worktree_add(repo.path(), &wt, "lyra/test-1", &base).unwrap();
        assert_eq!(current_branch(&wt).unwrap(), "lyra/test-1");
        assert!(is_clean(&wt).unwrap());

        std::fs::write(wt.join("b.txt"), "b\n").unwrap();
        std::fs::write(wt.join("a.txt"), "a2\n").unwrap();
        let ch = changed_files(&wt, &base).unwrap();
        assert!(ch.contains(&("M".to_string(), "a.txt".to_string())));
        assert!(ch.contains(&("??".to_string(), "b.txt".to_string())));

        let c2 = commit_all(&wt, "change").unwrap();
        assert_ne!(c2, base);
        let between = changed_files_between(&wt, &base, &c2).unwrap();
        assert!(between.contains(&("A".to_string(), "b.txt".to_string())));
        assert_eq!(show_file(&wt, &c2, "a.txt").unwrap(), "a2");

        std::fs::write(wt.join("junk.txt"), "x").unwrap();
        reset_hard_clean(&wt, &base).unwrap();
        assert_eq!(head(&wt).unwrap(), base);
        assert!(!wt.join("junk.txt").exists());
        assert!(!wt.join("b.txt").exists());
        assert!(is_clean(&wt).unwrap());

        assert_eq!(default_branch(repo.path()).unwrap(), "main");
        worktree_remove(repo.path(), &wt).unwrap();
    }

    #[test]
    fn ignored_files_are_checked_and_removed_on_reset() {
        let repo = init_repo();
        let base = head(repo.path()).unwrap();
        std::fs::write(repo.path().join(".gitignore"), ".cargo/\n").unwrap();
        std::fs::create_dir(repo.path().join(".cargo")).unwrap();
        std::fs::write(repo.path().join(".cargo/config.toml"), "[alias]\n").unwrap();
        assert!(changed_files(repo.path(), &base).unwrap().iter().any(|(_, p)| p == ".cargo/config.toml"));
        reset_hard_clean(repo.path(), &base).unwrap();
        assert!(!repo.path().join(".cargo").exists());
    }

    #[test]
    fn nul_status_preserves_unusual_paths() {
        assert_eq!(
            parse_name_status("M\0file\tname.rs\0A\0line\nname.rs\0"),
            vec![("M".into(), "file\tname.rs".into()), ("A".into(), "line\nname.rs".into())]
        );
    }

    #[test]
    fn commit_does_not_run_repository_hooks() {
        let repo = init_repo();
        let hooks = repo.path().join("hooks");
        std::fs::create_dir(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        run(repo.path(), &["config", "core.hooksPath", &hooks.to_string_lossy()]).unwrap();
        std::fs::write(repo.path().join("a.txt"), "changed").unwrap();
        commit_all(repo.path(), "hooks disabled").unwrap();
    }
}
