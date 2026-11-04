//! Guard rules for the repro, fix and verify stages (PLAN §3.1, §3.3, §3.4): which files each
//! stage may touch, the expected-assertion match, the test lock, and the parsing of
//! `cargo test` output.
//!
//! Everything here is string and path logic that runs without a repository; the two
//! `check_*_changes` entry points add the git / filesystem reads and delegate to the
//! `*_with` variants, which take the readers as closures so tests can feed them strings.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------------------
// Repro record
// ---------------------------------------------------------------------------------------

/// `20-repro/repro.json`: what the agent recorded about the failing test, plus what the
/// harness froze (`frozen_commit`, `frozen_len`). Matches `schemas/repro.schema.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReproRecord {
    pub reproduced: bool,
    /// Cargo package name (`cargo test -p <crate>`).
    #[serde(rename = "crate", default)]
    pub krate: String,
    /// Repository-relative path, forward slashes.
    #[serde(default)]
    pub test_file: String,
    /// Exactly as cargo prints it (`issue_123_x` or `parser::issue_123_tests::issue_123_x`).
    #[serde(default)]
    pub test_name: String,
    /// Always `cargo test -p <crate> <test_name> -- --exact` (rebuilt by the harness).
    #[serde(default)]
    pub command: String,
    /// Assertion message / panic text that proves the bug; must appear in the output.
    #[serde(default)]
    pub expected: String,
    #[serde(default)]
    pub observed: String,
    /// Source file that received the `#[cfg(test)] mod issue_<n>_tests;` line, if any.
    #[serde(default)]
    pub registration_file: Option<String>,
    #[serde(default)]
    pub notes: String,
    /// The S2 checkpoint commit: the frozen form of the test. Set by the harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_commit: Option<String>,
    /// Byte length of the test file at the frozen commit. Set by the harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_len: Option<u64>,
}

/// The one command the harness runs for the repro test, in S2 and again in S5.
pub fn repro_test_command(krate: &str, test_name: &str) -> String {
    format!("cargo test -p {} {} -- --exact", krate.trim(), test_name.trim())
}

// ---------------------------------------------------------------------------------------
// cargo test output
// ---------------------------------------------------------------------------------------

/// What `cargo test <name> -- --exact` did with the named test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestRun {
    /// The test binary did not build.
    CompileError,
    /// Everything was filtered out: the name is wrong or the test is not discovered.
    NotRun,
    Passed,
    Failed,
    Ignored,
    /// Anything else (cargo-level error, crashed test binary, no summary line), with the reason.
    Other(String),
}

/// One `test result:` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TestSummary {
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
    pub filtered: u64,
}

fn summary_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out")
            .unwrap()
    })
}

/// All `test result:` summary lines in the output (one per test target).
pub fn parse_summaries(output: &str) -> Vec<TestSummary> {
    summary_re()
        .captures_iter(output)
        .map(|c| {
            let n = |i: usize| c.get(i).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            TestSummary { passed: n(1), failed: n(2), ignored: n(3), filtered: n(5) }
        })
        .collect()
}

/// Classifies the output of `cargo test -p <crate> <test_name> -- --exact` for `test_name`.
/// A `test <name> ... <status>` line wins; otherwise build errors, then "0 tests ran".
pub fn classify_cargo_test(output: &str, test_name: &str) -> TestRun {
    let prefix = format!("test {} ... ", test_name.trim());
    let mut statuses: Vec<TestRun> = Vec::new();
    for line in output.lines().map(str::trim) {
        let Some(rest) = line.strip_prefix(prefix.as_str()) else {
            continue;
        };
        statuses.push(if rest.starts_with("ok") {
            TestRun::Passed
        } else if rest.starts_with("FAILED") {
            TestRun::Failed
        } else if rest.starts_with("ignored") {
            TestRun::Ignored
        } else {
            TestRun::Other(format!("unrecognised status for test {test_name}: {rest}"))
        });
    }
    if statuses.contains(&TestRun::Failed) {
        return TestRun::Failed;
    }
    if let Some(o) = statuses.iter().find(|s| matches!(s, TestRun::Other(_))) {
        return o.clone();
    }
    if statuses.contains(&TestRun::Passed) {
        return TestRun::Passed;
    }
    if statuses.contains(&TestRun::Ignored) {
        return TestRun::Ignored;
    }

    let mut first_error: Option<&str> = None;
    for line in output.lines().map(str::trim) {
        if line.starts_with("error[E") || line.starts_with("error: could not compile") {
            return TestRun::CompileError;
        }
        if line.starts_with("error") && !line.starts_with("error: test failed") && first_error.is_none() {
            first_error = Some(line);
        }
    }
    if let Some(e) = first_error {
        return TestRun::Other(e.to_string());
    }
    let summaries = parse_summaries(output);
    if summaries.is_empty() {
        return TestRun::Other("no `test result:` summary in the output".to_string());
    }
    if summaries.iter().all(|s| s.passed + s.failed + s.ignored == 0) {
        return TestRun::NotRun;
    }
    TestRun::Other(format!("test {test_name} did not appear in the output"))
}

/// Human-readable reason for a non-successful (or unexpected) classification.
pub fn describe_test_run(run: &TestRun, test_name: &str) -> String {
    match run {
        TestRun::CompileError => "the test did not compile (build error)".to_string(),
        TestRun::NotRun => format!("test {test_name} did not run (0 tests executed: filtered out or not discovered; check test_name against what cargo prints)"),
        TestRun::Passed => format!("test {test_name} passed"),
        TestRun::Failed => format!("test {test_name} FAILED"),
        TestRun::Ignored => format!("test {test_name} was ignored"),
        TestRun::Other(s) => s.clone(),
    }
}

/// Verify rule (PLAN §3.4 step 2): the named test passed and exactly one test ran.
pub fn assert_test_passed(output: &str, test_name: &str) -> Result<(), String> {
    match classify_cargo_test(output, test_name) {
        TestRun::Passed => {
            let summaries = parse_summaries(output);
            if summaries.is_empty() {
                return Err("no `test result:` summary in the output".to_string());
            }
            let passed: u64 = summaries.iter().map(|s| s.passed).sum();
            let failed: u64 = summaries.iter().map(|s| s.failed).sum();
            if passed == 1 && failed == 0 {
                Ok(())
            } else {
                Err(format!("expected exactly 1 passed test, summary shows {passed} passed / {failed} failed"))
            }
        }
        other => Err(describe_test_run(&other, test_name)),
    }
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Repro rule: the recorded `expected` text appears in the test output
/// (case-insensitive, whitespace-collapsed). An empty `expected` never matches.
pub fn output_contains_expected(output: &str, expected: &str) -> bool {
    let needle = squash(expected);
    !needle.is_empty() && squash(output).contains(&needle)
}

// ---------------------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------------------

/// Forward slashes, no leading `./` or `/`.
pub fn norm_path(p: &str) -> String {
    let p = p.trim().replace('\\', "/");
    let p = p.strip_prefix("./").unwrap_or(&p);
    p.trim_matches('/').to_string()
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// True when a directory component (not the file name) equals `name`.
fn has_dir(path: &str, name: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    parts.len() > 1 && parts[..parts.len() - 1].contains(&name)
}

fn is_new(status: &str) -> bool {
    matches!(status, "??" | "A")
}

/// `issue_<n>` in a file name (`tests/issue_123.rs`, `src/p/issue_123_tests.rs`).
pub fn issue_number_in_path(path: &str) -> Option<u64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"issue_(\d+)").unwrap());
    re.captures(base_name(path)).and_then(|c| c[1].parse().ok())
}

/// A path that cargo or convention treats as test code.
pub fn is_test_path(path: &str) -> bool {
    let name = base_name(path);
    has_dir(path, "tests")
        || has_dir(path, "benches")
        || name.ends_with("_test.rs")
        || name.ends_with("_tests.rs")
        || name == "tests.rs"
        || name == "test.rs"
}

// ---------------------------------------------------------------------------------------
// Repro diff rules (PLAN §3.1)
// ---------------------------------------------------------------------------------------

/// Allowed S2 changes: the new test file, new files under `tests/` or `fixtures/`, `Cargo.lock`,
/// `Cargo.toml` with only `[dev-dependencies]` changed, and the registration file with only the
/// `#[cfg(test)] mod issue_<n>_tests;` line added. Anything else is listed in the error.
/// `changes` is `git::changed_files(worktree, base_commit)`.
pub fn check_repro_changes(
    changes: &[(String, String)],
    rec: &ReproRecord,
    worktree: &Path,
    base_commit: &str,
) -> Result<(), String> {
    let read_old = |p: &str| crate::git::show_file(worktree, base_commit, p).ok();
    let read_new = |p: &str| std::fs::read_to_string(worktree.join(p)).ok();
    check_repro_changes_with(changes, rec, &read_old, &read_new)
}

/// `check_repro_changes` with the file readers supplied (`read_old` = content at the base
/// commit, `read_new` = content in the worktree; `None` when the file does not exist).
pub fn check_repro_changes_with(
    changes: &[(String, String)],
    rec: &ReproRecord,
    read_old: &dyn Fn(&str) -> Option<String>,
    read_new: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let test_file = norm_path(&rec.test_file);
    let reg_file = rec.registration_file.as_deref().map(norm_path).filter(|s| !s.is_empty());
    let mut violations: Vec<String> = Vec::new();
    let mut saw_test_file = false;

    for (status, raw) in changes {
        let path = norm_path(raw);
        let status = status.as_str();
        if path == test_file {
            saw_test_file = true;
            if !is_new(status) {
                violations.push(format!("{status} {path}: the test file must be a new file, not a modified one"));
            }
            continue;
        }
        let name = base_name(&path);
        if name == "Cargo.lock" {
            continue;
        }
        if is_new(status) && (has_dir(&path, "tests") || has_dir(&path, "fixtures")) {
            continue;
        }
        if name == "Cargo.toml" && status == "M" {
            let old = read_old(&path).unwrap_or_default();
            let new = read_new(&path).unwrap_or_default();
            if cargo_toml_only_dev_deps_changed(&old, &new) {
                continue;
            }
            violations.push(format!("{status} {path}: only [dev-dependencies] may change in the repro stage"));
            continue;
        }
        if reg_file.as_deref() == Some(path.as_str()) && status == "M" {
            let old = read_old(&path).unwrap_or_default();
            let new = read_new(&path).unwrap_or_default();
            if is_registration_only_change(&old, &new) {
                continue;
            }
            violations.push(format!(
                "{status} {path}: only the `#[cfg(test)] mod issue_<n>_tests;` registration line may be added"
            ));
            continue;
        }
        violations.push(format!("{status} {path}: not allowed in the repro stage"));
    }

    if !saw_test_file {
        violations.insert(0, format!("the test file {test_file} was not created"));
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!("repro changes outside the allowed set: {}", violations.join("; ")))
    }
}

// ---------------------------------------------------------------------------------------
// Cargo.toml helpers (text based, so a broken file still compares)
// ---------------------------------------------------------------------------------------

/// Splits a TOML file into `(header, non-blank trimmed lines)`; the preamble has header `""`.
/// Headers keep their brackets (`[dev-dependencies]`, `[[test]]`).
fn toml_sections(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = vec![(String::new(), Vec::new())];
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            let head = line.split('#').next().unwrap_or("").trim();
            if head.ends_with(']') {
                out.push((head.to_string(), Vec::new()));
                continue;
            }
        }
        out.last_mut().expect("preamble").1.push(line.to_string());
    }
    out
}

fn is_dev_deps_header(header: &str) -> bool {
    let key = header.trim_matches(|c| c == '[' || c == ']').trim();
    key == "dev-dependencies" || key.ends_with(".dev-dependencies")
}

/// True when `old` and `new` differ at most inside `[dev-dependencies]`
/// (and `[target.*.dev-dependencies]`) sections. Comments count; blank lines do not.
pub fn cargo_toml_only_dev_deps_changed(old: &str, new: &str) -> bool {
    let rest = |t: &str| toml_sections(t).into_iter().filter(|(h, _)| !is_dev_deps_header(h)).collect::<Vec<_>>();
    rest(old) == rest(new)
}

/// Fix rule: `[[test]]` sections, `autotests` keys and `[dev-dependencies]` are unchanged.
pub fn cargo_toml_test_sections_equal(old: &str, new: &str) -> bool {
    type Extract = (Vec<Vec<String>>, Vec<Vec<String>>, Vec<String>);
    fn extract(t: &str) -> Extract {
        let secs = toml_sections(t);
        let tests = secs.iter().filter(|(h, _)| h == "[[test]]").map(|(_, b)| b.clone()).collect();
        let dev = secs.iter().filter(|(h, _)| is_dev_deps_header(h)).map(|(_, b)| b.clone()).collect();
        let auto = secs.iter().flat_map(|(_, b)| b.iter()).filter(|l| l.starts_with("autotests")).cloned().collect();
        (tests, dev, auto)
    }
    extract(old) == extract(new)
}

// ---------------------------------------------------------------------------------------
// Registration line
// ---------------------------------------------------------------------------------------

fn cfg_test_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*#\[cfg\(test\)\]\s*$").unwrap())
}
fn mod_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*mod issue_(\d+)_tests;\s*$").unwrap())
}
fn one_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*#\[cfg\(test\)\]\s*mod issue_(\d+)_tests;\s*$").unwrap())
}

fn mod_line_issue(line: &str) -> Option<u64> {
    mod_line_re().captures(line).and_then(|c| c[1].parse().ok())
}
fn one_line_issue(line: &str) -> Option<u64> {
    one_line_re().captures(line).and_then(|c| c[1].parse().ok())
}

/// `#[cfg(test)]`, `mod issue_<n>_tests;`, or both on one line.
pub fn is_registration_line(line: &str) -> bool {
    cfg_test_re().is_match(line) || mod_line_issue(line).is_some() || one_line_issue(line).is_some()
}

/// True when `new` is `old` plus inserted lines that are all registration lines (or blank).
/// No line of `old` may be removed or altered.
pub fn is_registration_only_change(old: &str, new: &str) -> bool {
    let old: Vec<&str> = old.lines().map(str::trim_end).collect();
    let mut i = 0;
    for line in new.lines().map(str::trim_end) {
        if i < old.len() && line == old[i] {
            i += 1;
            continue;
        }
        if line.trim().is_empty() || is_registration_line(line) {
            continue;
        }
        return false;
    }
    i == old.len()
}

/// True when `content` registers `issue_<n>_tests` under `#[cfg(test)]` (one line, or the
/// attribute on the previous non-blank line). `issue = None` accepts any issue number.
pub fn has_registration_line(content: &str, issue: Option<u64>) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    let wanted = |n: u64| issue.is_none_or(|i| i == n);
    for (idx, line) in lines.iter().enumerate() {
        if let Some(n) = one_line_issue(line) {
            if wanted(n) {
                return true;
            }
        }
        if let Some(n) = mod_line_issue(line) {
            if !wanted(n) {
                continue;
            }
            let prev = lines[..idx].iter().rev().find(|p| !p.trim().is_empty());
            if prev.is_some_and(|p| cfg_test_re().is_match(p)) {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------------------
// Fix rules (PLAN §3.3): the test is locked
// ---------------------------------------------------------------------------------------

/// Rejects a fix that touched the frozen test, its registration line, `Cargo.toml` test
/// sections, `.cargo/`, or the recorded command. `changes` is
/// `git::changed_files(worktree, checkpoint)` where `checkpoint` is the S2 commit.
pub fn check_fix_changes(
    changes: &[(String, String)],
    rec: &ReproRecord,
    worktree: &Path,
    checkpoint: &str,
) -> Result<(), String> {
    let read_checkpoint = |p: &str| crate::git::show_file(worktree, checkpoint, p).ok();
    let read_now = |p: &str| std::fs::read_to_string(worktree.join(p)).ok();
    check_fix_changes_with(changes, rec, &read_checkpoint, &read_now)
}

fn normalise_text(s: &str) -> String {
    s.replace("\r\n", "\n").trim_end().to_string()
}

/// `check_fix_changes` with the readers supplied (`read_checkpoint` = content at the S2
/// commit, `read_now` = content in the worktree).
pub fn check_fix_changes_with(
    changes: &[(String, String)],
    rec: &ReproRecord,
    read_checkpoint: &dyn Fn(&str) -> Option<String>,
    read_now: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    if changes.is_empty() {
        return Err("fix produced no changes".to_string());
    }
    let mut violations: Vec<String> = Vec::new();
    let test_file = norm_path(&rec.test_file);
    let issue = issue_number_in_path(&test_file);

    // 1. The test file is byte-for-byte its frozen form (git's own comparison first, then the
    //    content after newline normalisation, because `git show` trims and autocrlf may differ).
    if changes.iter().any(|(_, p)| norm_path(p) == test_file) {
        violations.push(format!("{test_file}: the repro test file was modified or removed (it is locked)"));
    } else {
        match (read_checkpoint(&test_file), read_now(&test_file)) {
            (Some(frozen), Some(now)) if normalise_text(&frozen) != normalise_text(&now) => {
                violations.push(format!("{test_file}: content differs from its frozen form"));
            }
            (Some(_), None) => violations.push(format!("{test_file}: the repro test file was deleted")),
            _ => {}
        }
    }

    // 2. The registration line is still there.
    if let Some(reg) = rec.registration_file.as_deref().map(norm_path).filter(|s| !s.is_empty()) {
        match read_now(&reg) {
            Some(c) if has_registration_line(&c, issue) => {}
            Some(_) => violations.push(format!(
                "{reg}: the `#[cfg(test)] mod issue_<n>_tests;` registration line was removed or changed"
            )),
            None => violations.push(format!("{reg}: the registration file was deleted")),
        }
    }

    // 3. Per-file rules.
    for (status, raw) in changes {
        let path = norm_path(raw);
        if path.split('/').any(|c| c == ".cargo") {
            violations.push(format!("{status} {path}: .cargo/ configuration must not change"));
            continue;
        }
        if base_name(&path) == "Cargo.toml" {
            let old = read_checkpoint(&path).unwrap_or_default();
            let new = read_now(&path).unwrap_or_default();
            if !cargo_toml_test_sections_equal(&old, &new) {
                violations.push(format!("{status} {path}: [[test]], autotests or [dev-dependencies] changed"));
            }
        }
    }

    // 4. The recorded command is our constant.
    let expected_cmd = repro_test_command(&rec.krate, &rec.test_name);
    if rec.command.trim() != expected_cmd {
        violations.push(format!("recorded test command changed: {:?} (expected {expected_cmd:?})", rec.command.trim()));
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!("fix violates the test lock: {}", violations.join("; ")))
    }
}

// ---------------------------------------------------------------------------------------
// Scope of a fix (40-fix/scope.json)
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeFile {
    pub path: String,
    pub added: u64,
    pub removed: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub files: Vec<ScopeFile>,
    pub total_added: u64,
    pub total_removed: u64,
    /// Changed paths that look like test code (the locked test itself is never among them).
    pub test_files: Vec<String>,
    pub warnings: Vec<String>,
}

/// Lines changed per file is considered large above this.
pub const LARGE_CHANGE_LINES: u64 = 100;

fn matches_culprit(path: &str, culprits: &[String]) -> bool {
    culprits.iter().any(|c| {
        let c = norm_path(c);
        !c.is_empty() && (c == path || path.ends_with(&format!("/{c}")) || c.ends_with(&format!("/{path}")))
    })
}

/// Per-file added/removed counts from a unified diff, with warnings for a large change, files
/// the RCA did not name (`culprits`, repository-relative; empty = no check) and test files touched.
pub fn scope_from_diff(diff: &str, culprits: &[String]) -> Scope {
    let mut scope = Scope::default();
    let mut current: Option<ScopeFile> = None;
    let mut in_hunk = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(f) = current.take() {
                scope.files.push(f);
            }
            let path = rest.find(" b/").map(|i| &rest[i + 3..]).unwrap_or(rest);
            current = Some(ScopeFile { path: norm_path(path.trim_matches('"')), added: 0, removed: 0 });
            in_hunk = false;
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if let Some(f) = current.as_mut() {
            if line.starts_with('+') {
                f.added += 1;
            } else if line.starts_with('-') {
                f.removed += 1;
            }
        }
    }
    if let Some(f) = current.take() {
        scope.files.push(f);
    }

    scope.total_added = scope.files.iter().map(|f| f.added).sum();
    scope.total_removed = scope.files.iter().map(|f| f.removed).sum();
    let total = scope.total_added + scope.total_removed;
    if total > LARGE_CHANGE_LINES {
        scope.warnings.push(format!(
            "large change: {total} lines changed ({} added, {} removed)",
            scope.total_added, scope.total_removed
        ));
    }
    for f in &scope.files {
        if is_test_path(&f.path) {
            scope.test_files.push(f.path.clone());
            scope.warnings.push(format!("{}: test file touched", f.path));
        }
        if !culprits.is_empty() && !matches_culprit(&f.path, culprits) {
            scope.warnings.push(format!("{}: not named by the RCA culprits", f.path));
        }
    }
    scope
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const NAME: &str = "issue_123_rejects_short_frame";

    const FAILED_OUTPUT: &str = "\
   Compiling foo v0.1.0 (C:\\src\\foo)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.23s
     Running unittests src/lib.rs (target/debug/deps/foo-1111)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 12 filtered out; finished in 0.00s

     Running tests/issue_123.rs (target/debug/deps/issue_123-2222)

running 1 test
test issue_123_rejects_short_frame ... FAILED

failures:

---- issue_123_rejects_short_frame stdout ----
thread 'issue_123_rejects_short_frame' panicked at tests/issue_123.rs:9:5:
assertion `left == right` failed: short frame must be rejected
  left: Ok(Frame { len: 2 })
 right: Err(TooShort)
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    issue_123_rejects_short_frame

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

--- stderr ---
error: test failed, to rerun pass `--test issue_123`
";

    const PASSED_OUTPUT: &str = "\
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.10s
     Running unittests src/lib.rs (target/debug/deps/foo-1111)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 12 filtered out; finished in 0.00s

     Running tests/issue_123.rs (target/debug/deps/issue_123-2222)

running 1 test
test issue_123_rejects_short_frame ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";

    const COMPILE_ERROR_OUTPUT: &str = "\
   Compiling foo v0.1.0 (C:\\src\\foo)
--- stderr ---
error[E0425]: cannot find function `parse_frame` in this scope
 --> tests/issue_123.rs:5:13
  |
5 |     let r = parse_frame(&[0u8; 2]);
  |             ^^^^^^^^^^^ not found in this scope

error: could not compile `foo` (test \"issue_123\") due to 1 previous error
";

    const FILTERED_OUTPUT: &str = "\
     Running unittests src/lib.rs (target/debug/deps/foo-1111)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 12 filtered out; finished in 0.00s

     Running tests/issue_123.rs (target/debug/deps/issue_123-2222)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s
";

    const IGNORED_OUTPUT: &str = "\
running 1 test
test issue_123_rejects_short_frame ... ignored, needs hardware

test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
";

    #[test]
    fn classify_failed_with_panic_message() {
        assert_eq!(classify_cargo_test(FAILED_OUTPUT, NAME), TestRun::Failed);
        assert!(output_contains_expected(FAILED_OUTPUT, "short frame must be rejected"));
        assert!(output_contains_expected(FAILED_OUTPUT, "  SHORT frame   must be rejected "));
        assert!(output_contains_expected(FAILED_OUTPUT, "left: Ok(Frame { len: 2 })\n right: Err(TooShort)"));
        assert!(!output_contains_expected(FAILED_OUTPUT, "something else entirely"));
        assert!(!output_contains_expected(FAILED_OUTPUT, "   "));
        assert!(assert_test_passed(FAILED_OUTPUT, NAME).unwrap_err().contains("FAILED"));
    }

    #[test]
    fn classify_passed() {
        assert_eq!(classify_cargo_test(PASSED_OUTPUT, NAME), TestRun::Passed);
        assert_eq!(assert_test_passed(PASSED_OUTPUT, NAME), Ok(()));
        // A module test name is matched exactly as printed.
        let modpath = PASSED_OUTPUT.replace(NAME, "parser::issue_123_tests::issue_123_rejects_short_frame");
        assert_eq!(
            classify_cargo_test(&modpath, "parser::issue_123_tests::issue_123_rejects_short_frame"),
            TestRun::Passed
        );
        assert_eq!(
            classify_cargo_test(&modpath, NAME),
            TestRun::Other(format!("test {NAME} did not appear in the output"))
        );
    }

    #[test]
    fn classify_compile_error() {
        assert_eq!(classify_cargo_test(COMPILE_ERROR_OUTPUT, NAME), TestRun::CompileError);
        assert!(assert_test_passed(COMPILE_ERROR_OUTPUT, NAME).unwrap_err().contains("compile"));
        assert_eq!(
            classify_cargo_test("error: could not compile `foo` due to 2 previous errors\n", NAME),
            TestRun::CompileError
        );
    }

    #[test]
    fn classify_filtered_and_ignored() {
        assert_eq!(classify_cargo_test(FILTERED_OUTPUT, NAME), TestRun::NotRun);
        assert!(assert_test_passed(FILTERED_OUTPUT, NAME).unwrap_err().contains("did not run"));
        assert_eq!(classify_cargo_test(IGNORED_OUTPUT, NAME), TestRun::Ignored);
        assert!(assert_test_passed(IGNORED_OUTPUT, NAME).unwrap_err().contains("ignored"));
    }

    #[test]
    fn classify_cargo_level_errors_and_empty_output() {
        let out = "error: package ID specification `nope` did not match any packages\n";
        assert_eq!(classify_cargo_test(out, NAME), TestRun::Other(out.trim().to_string()));
        assert!(matches!(classify_cargo_test("", NAME), TestRun::Other(_)));
        let crash =
            "running 1 test\nerror: process didn't exit successfully: `target/debug/deps/x` (exit code: 0xc0000005)\n";
        assert!(
            matches!(classify_cargo_test(crash, NAME), TestRun::Other(s) if s.contains("didn't exit successfully"))
        );
    }

    #[test]
    fn assert_test_passed_requires_exactly_one() {
        let two = PASSED_OUTPUT.replace("1 passed", "2 passed");
        assert!(assert_test_passed(&two, NAME).unwrap_err().contains("exactly 1"));
        let no_summary = "running 1 test\ntest issue_123_rejects_short_frame ... ok\n";
        assert!(assert_test_passed(no_summary, NAME).unwrap_err().contains("summary"));
        let summaries = parse_summaries(FAILED_OUTPUT);
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[1].failed, 1);
        assert_eq!(summaries[0].filtered, 12);
    }

    #[test]
    fn command_is_built_from_crate_and_name() {
        assert_eq!(repro_test_command(" foo ", NAME), format!("cargo test -p foo {NAME} -- --exact"));
    }

    #[test]
    fn repro_record_roundtrips_and_tolerates_extras() {
        let json = r#"{"reproduced":true,"crate":"foo","test_file":"tests/issue_123.rs","test_name":"issue_123_x",
            "command":"cargo test -p foo issue_123_x -- --exact","expected":"boom","observed":"it boomed",
            "registration_file":null,"notes":"n","frozen_commit":"abc","unknown_extra":1}"#;
        let rec: ReproRecord = serde_json::from_str(json).unwrap();
        assert_eq!(rec.krate, "foo");
        assert_eq!(rec.registration_file, None);
        assert_eq!(rec.frozen_commit.as_deref(), Some("abc"));
        let text = serde_json::to_string(&rec).unwrap();
        assert!(text.contains("\"crate\":\"foo\""));
        assert!(!text.contains("frozen_len"));
        let back: ReproRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(back, rec);
        // `reproduced` is the one field that must be present.
        assert!(serde_json::from_str::<ReproRecord>(r#"{"crate":"foo"}"#).is_err());
        let minimal: ReproRecord = serde_json::from_str(r#"{"reproduced":false}"#).unwrap();
        assert!(!minimal.reproduced);
        assert_eq!(minimal.test_file, "");
    }

    fn rec() -> ReproRecord {
        ReproRecord {
            reproduced: true,
            krate: "foo".into(),
            test_file: "tests/issue_123.rs".into(),
            test_name: NAME.into(),
            command: repro_test_command("foo", NAME),
            expected: "short frame must be rejected".into(),
            observed: String::new(),
            registration_file: None,
            notes: String::new(),
            frozen_commit: None,
            frozen_len: None,
        }
    }

    fn reader<'a>(map: &'a HashMap<&'a str, &'a str>) -> impl Fn(&str) -> Option<String> + 'a {
        move |p: &str| map.get(p).map(|s| s.to_string())
    }

    const CARGO_OLD: &str = "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n";
    const CARGO_DEV: &str = "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n\n[dev-dependencies]\nproptest = \"1\"\n";
    const CARGO_DEP: &str =
        "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\nbytes = \"1\"\n";
    const LIB_OLD: &str = "pub mod parser;\n\npub fn x() {}\n";
    const LIB_REG: &str = "pub mod parser;\n\n#[cfg(test)]\nmod issue_123_tests;\n\npub fn x() {}\n";

    #[test]
    fn repro_changes_allowed_set() {
        let changes = vec![
            ("??".to_string(), "tests/issue_123.rs".to_string()),
            ("??".to_string(), "tests/fixtures/short.bin".to_string()),
            ("A".to_string(), "crates/bar/fixtures/data.json".to_string()),
            ("M".to_string(), "Cargo.toml".to_string()),
            ("M".to_string(), "Cargo.lock".to_string()),
        ];
        let old: HashMap<&str, &str> = HashMap::from([("Cargo.toml", CARGO_OLD)]);
        let new: HashMap<&str, &str> = HashMap::from([("Cargo.toml", CARGO_DEV)]);
        assert_eq!(check_repro_changes_with(&changes, &rec(), &reader(&old), &reader(&new)), Ok(()));
    }

    #[test]
    fn repro_changes_module_test_with_registration() {
        let mut r = rec();
        r.test_file = "src/parser/issue_123_tests.rs".into();
        r.registration_file = Some("src/lib.rs".into());
        let changes = vec![
            ("??".to_string(), "src/parser/issue_123_tests.rs".to_string()),
            ("M".to_string(), "src/lib.rs".to_string()),
        ];
        let old: HashMap<&str, &str> = HashMap::from([("src/lib.rs", LIB_OLD)]);
        let new: HashMap<&str, &str> = HashMap::from([("src/lib.rs", LIB_REG)]);
        assert_eq!(check_repro_changes_with(&changes, &r, &reader(&old), &reader(&new)), Ok(()));
        // Same file with real code added next to the registration line.
        let sneaky = "pub mod parser;\n\n#[cfg(test)]\nmod issue_123_tests;\n\npub fn x() { fixed() }\n";
        let new2: HashMap<&str, &str> = HashMap::from([("src/lib.rs", sneaky)]);
        let err = check_repro_changes_with(&changes, &r, &reader(&old), &reader(&new2)).unwrap_err();
        assert!(err.contains("src/lib.rs"), "{err}");
    }

    #[test]
    fn repro_changes_forbidden_combos() {
        let old: HashMap<&str, &str> = HashMap::from([("Cargo.toml", CARGO_OLD), ("src/lib.rs", LIB_OLD)]);
        let new: HashMap<&str, &str> = HashMap::from([("Cargo.toml", CARGO_DEP), ("src/lib.rs", LIB_REG)]);
        let ok_test = ("??".to_string(), "tests/issue_123.rs".to_string());

        // Production code edited.
        let changes = vec![ok_test.clone(), ("M".to_string(), "src/parser.rs".to_string())];
        let err = check_repro_changes_with(&changes, &rec(), &reader(&old), &reader(&new)).unwrap_err();
        assert!(err.contains("M src/parser.rs"), "{err}");
        assert!(!err.contains("tests/issue_123.rs"), "{err}");

        // Test file never created.
        let err = check_repro_changes_with(&[], &rec(), &reader(&old), &reader(&new)).unwrap_err();
        assert!(err.contains("was not created"), "{err}");

        // Test file is a modified existing file.
        let changes = vec![("M".to_string(), "tests/issue_123.rs".to_string())];
        let err = check_repro_changes_with(&changes, &rec(), &reader(&old), &reader(&new)).unwrap_err();
        assert!(err.contains("must be a new file"), "{err}");

        // Cargo.toml [dependencies] changed.
        let changes = vec![ok_test.clone(), ("M".to_string(), "Cargo.toml".to_string())];
        let err = check_repro_changes_with(&changes, &rec(), &reader(&old), &reader(&new)).unwrap_err();
        assert!(err.contains("[dev-dependencies]"), "{err}");

        // Registration-style edit to a file the record does not name, and a deletion.
        let changes = vec![
            ok_test.clone(),
            ("M".to_string(), "src/lib.rs".to_string()),
            ("D".to_string(), "src/old.rs".to_string()),
        ];
        let err = check_repro_changes_with(&changes, &rec(), &reader(&old), &reader(&new)).unwrap_err();
        assert!(err.contains("M src/lib.rs") && err.contains("D src/old.rs"), "{err}");

        // Modified fixture (only new fixtures are allowed).
        let changes = vec![ok_test, ("M".to_string(), "tests/fixtures/a.bin".to_string())];
        assert!(check_repro_changes_with(&changes, &rec(), &reader(&old), &reader(&new)).is_err());
    }

    #[test]
    fn cargo_toml_dev_deps_rule() {
        assert!(cargo_toml_only_dev_deps_changed(CARGO_OLD, CARGO_OLD));
        assert!(cargo_toml_only_dev_deps_changed(CARGO_OLD, CARGO_DEV));
        assert!(cargo_toml_only_dev_deps_changed(
            CARGO_DEV,
            &CARGO_DEV.replace("proptest = \"1\"", "proptest = \"1\"\ntempfile = \"3\"")
        ));
        assert!(cargo_toml_only_dev_deps_changed(
            CARGO_OLD,
            &format!("{CARGO_OLD}\n[target.'cfg(unix)'.dev-dependencies]\nnix = \"0.29\"\n")
        ));
        assert!(!cargo_toml_only_dev_deps_changed(CARGO_OLD, CARGO_DEP));
        assert!(!cargo_toml_only_dev_deps_changed(
            CARGO_OLD,
            &format!("{CARGO_OLD}\n[[test]]\nname = \"x\"\nharness = false\n")
        ));
        assert!(!cargo_toml_only_dev_deps_changed(CARGO_OLD, &CARGO_OLD.replace("0.1.0", "0.2.0")));
        // Blank lines and whitespace are not changes; a header comment is tolerated.
        assert!(cargo_toml_only_dev_deps_changed(
            CARGO_OLD,
            &CARGO_OLD.replace("\n[dependencies]", "\n\n  [dependencies]  # deps")
        ));
    }

    #[test]
    fn cargo_toml_test_sections_rule() {
        assert!(cargo_toml_test_sections_equal(CARGO_OLD, CARGO_DEP));
        assert!(cargo_toml_test_sections_equal(CARGO_DEV, &CARGO_DEV.replace("serde = \"1\"", "serde = \"1.0.200\"")));
        assert!(!cargo_toml_test_sections_equal(CARGO_OLD, CARGO_DEV));
        assert!(!cargo_toml_test_sections_equal(
            CARGO_OLD,
            &format!("{CARGO_OLD}\n[[test]]\nname = \"issue_123\"\npath = \"other.rs\"\n")
        ));
        assert!(!cargo_toml_test_sections_equal(
            CARGO_OLD,
            &CARGO_OLD.replace("version = \"0.1.0\"", "version = \"0.1.0\"\nautotests = false")
        ));
        assert!(!cargo_toml_test_sections_equal("", CARGO_DEV));
    }

    #[test]
    fn registration_only_change_rule() {
        assert!(is_registration_only_change(LIB_OLD, LIB_OLD));
        assert!(is_registration_only_change(LIB_OLD, LIB_REG));
        assert!(is_registration_only_change(
            LIB_OLD,
            "pub mod parser;\n#[cfg(test)] mod issue_123_tests;\n\npub fn x() {}\n"
        ));
        assert!(is_registration_only_change(
            LIB_OLD,
            "pub mod parser;\n\npub fn x() {}\n\n#[cfg(test)]\nmod issue_7_tests;\n"
        ));
        assert!(is_registration_only_change(
            "pub mod parser;\r\n",
            "pub mod parser;\r\n#[cfg(test)]\r\nmod issue_1_tests;\r\n"
        ));
        assert!(!is_registration_only_change(
            LIB_OLD,
            "pub mod parser;\n\n#[cfg(test)]\nmod other_tests;\n\npub fn x() {}\n"
        ));
        assert!(!is_registration_only_change(
            LIB_OLD,
            "pub mod parser;\n\n#[cfg(test)]\nmod issue_123_tests;\n\npub fn x() { 1 }\n"
        ));
        assert!(!is_registration_only_change(LIB_OLD, "#[cfg(test)]\nmod issue_123_tests;\n\npub fn x() {}\n"));
        assert!(!is_registration_only_change(
            LIB_OLD,
            "pub mod parser;\n\n#[cfg(test)]\nmod issue_123_tests;\nuse std::fs;\n\npub fn x() {}\n"
        ));
        assert!(is_registration_line("    #[cfg(test)]"));
        assert!(is_registration_line("mod issue_9_tests;"));
        assert!(!is_registration_line("mod issue_9_tests; // x"));
    }

    #[test]
    fn registration_presence() {
        assert!(has_registration_line(LIB_REG, Some(123)));
        assert!(has_registration_line(LIB_REG, None));
        assert!(!has_registration_line(LIB_REG, Some(124)));
        assert!(has_registration_line("#[cfg(test)] mod issue_5_tests;\n", Some(5)));
        assert!(has_registration_line("#[cfg(test)]\n\n  mod issue_5_tests;\n", Some(5)));
        assert!(!has_registration_line("mod issue_5_tests;\n", Some(5)), "cfg(test) gate removed");
        assert!(!has_registration_line("#[cfg(test)]\nmod tests;\nmod issue_5_tests;\n", Some(5)));
        assert!(!has_registration_line(LIB_OLD, None));
    }

    #[test]
    fn fix_changes_detect_test_lock_violations() {
        let frozen = "use foo::parse;\n#[test]\nfn issue_123_rejects_short_frame() { assert!(parse(&[0,1]).is_err(), \"short frame must be rejected\"); }\n";
        let base: HashMap<&str, &str> =
            HashMap::from([("tests/issue_123.rs", frozen), ("Cargo.toml", CARGO_DEV), ("src/lib.rs", LIB_REG)]);

        // Clean fix: production file only.
        let now: HashMap<&str, &str> =
            HashMap::from([("tests/issue_123.rs", frozen), ("Cargo.toml", CARGO_DEV), ("src/lib.rs", LIB_REG)]);
        let changes = vec![("M".to_string(), "src/parser.rs".to_string())];
        assert_eq!(check_fix_changes_with(&changes, &rec(), &reader(&base), &reader(&now)), Ok(()));

        // Test file modified (reported by git).
        let changes =
            vec![("M".to_string(), "src/parser.rs".to_string()), ("M".to_string(), "tests/issue_123.rs".to_string())];
        let err = check_fix_changes_with(&changes, &rec(), &reader(&base), &reader(&now)).unwrap_err();
        assert!(err.contains("locked"), "{err}");

        // Test file content drifted (e.g. #[ignore] added) even if the list missed it.
        let ignored = frozen.replace("#[test]", "#[test]\n#[ignore]");
        let now2: HashMap<&str, &str> = HashMap::from([
            ("tests/issue_123.rs", ignored.as_str()),
            ("Cargo.toml", CARGO_DEV),
            ("src/lib.rs", LIB_REG),
        ]);
        let changes = vec![("M".to_string(), "src/parser.rs".to_string())];
        let err = check_fix_changes_with(&changes, &rec(), &reader(&base), &reader(&now2)).unwrap_err();
        assert!(err.contains("frozen form"), "{err}");

        // CRLF / trailing newline differences alone are not drift.
        let crlf = format!("{}\r\n", frozen.replace('\n', "\r\n"));
        let now3: HashMap<&str, &str> =
            HashMap::from([("tests/issue_123.rs", crlf.as_str()), ("Cargo.toml", CARGO_DEV), ("src/lib.rs", LIB_REG)]);
        assert_eq!(check_fix_changes_with(&changes, &rec(), &reader(&base), &reader(&now3)), Ok(()));

        // Test file deleted.
        let now4: HashMap<&str, &str> = HashMap::from([("Cargo.toml", CARGO_DEV)]);
        let err = check_fix_changes_with(&changes, &rec(), &reader(&base), &reader(&now4)).unwrap_err();
        assert!(err.contains("deleted"), "{err}");

        // Registration line removed.
        let mut r = rec();
        r.test_file = "src/parser/issue_123_tests.rs".into();
        r.registration_file = Some("src/lib.rs".into());
        let base2: HashMap<&str, &str> =
            HashMap::from([("src/parser/issue_123_tests.rs", frozen), ("src/lib.rs", LIB_REG)]);
        let now5: HashMap<&str, &str> =
            HashMap::from([("src/parser/issue_123_tests.rs", frozen), ("src/lib.rs", LIB_OLD)]);
        let changes = vec![("M".to_string(), "src/lib.rs".to_string())];
        let err = check_fix_changes_with(&changes, &r, &reader(&base2), &reader(&now5)).unwrap_err();
        assert!(err.contains("registration line"), "{err}");
        // The registration file may change elsewhere as long as the line stays.
        let lib_fixed = LIB_REG.replace("pub fn x() {}", "pub fn x() { fixed() }");
        let now6: HashMap<&str, &str> =
            HashMap::from([("src/parser/issue_123_tests.rs", frozen), ("src/lib.rs", lib_fixed.as_str())]);
        assert_eq!(check_fix_changes_with(&changes, &r, &reader(&base2), &reader(&now6)), Ok(()));

        // Cargo.toml test sections, .cargo/, command, empty diff.
        let with_test = format!("{CARGO_DEV}\n[[test]]\nname = \"issue_123\"\nharness = false\n");
        let now7: HashMap<&str, &str> =
            HashMap::from([("tests/issue_123.rs", frozen), ("Cargo.toml", with_test.as_str())]);
        let changes =
            vec![("M".to_string(), "Cargo.toml".to_string()), ("??".to_string(), ".cargo/config.toml".to_string())];
        let err = check_fix_changes_with(&changes, &rec(), &reader(&base), &reader(&now7)).unwrap_err();
        assert!(err.contains("[[test]]") && err.contains(".cargo/"), "{err}");
        let mut r2 = rec();
        r2.command = "cargo test".into();
        let changes = vec![("M".to_string(), "src/parser.rs".to_string())];
        let err = check_fix_changes_with(&changes, &r2, &reader(&base), &reader(&now)).unwrap_err();
        assert!(err.contains("command changed"), "{err}");
        assert_eq!(
            check_fix_changes_with(&[], &rec(), &reader(&base), &reader(&now)).unwrap_err(),
            "fix produced no changes"
        );
    }

    const SAMPLE_DIFF: &str = "\
diff --git a/src/parser.rs b/src/parser.rs
index 1111111..2222222 100644
--- a/src/parser.rs
+++ b/src/parser.rs
@@ -10,7 +10,9 @@ pub fn parse_header(buf: &[u8]) -> Result<Header, Error> {
-    if buf.len() < 2 {
+    if buf.len() < 4 {
+        // +++ not a header
         return Err(Error::TooShort);
     }
+    let _ = buf[3];
diff --git a/tests/common/mod.rs b/tests/common/mod.rs
new file mode 100644
--- /dev/null
+++ b/tests/common/mod.rs
@@ -0,0 +1,2 @@
+pub fn helper() {}
+--- not a header either
\\ No newline at end of file
";

    #[test]
    fn scope_counts_two_files_and_warns() {
        let scope = scope_from_diff(SAMPLE_DIFF, &["src/parser.rs".to_string()]);
        assert_eq!(scope.files.len(), 2);
        assert_eq!(scope.files[0], ScopeFile { path: "src/parser.rs".into(), added: 3, removed: 1 });
        assert_eq!(scope.files[1], ScopeFile { path: "tests/common/mod.rs".into(), added: 2, removed: 0 });
        assert_eq!((scope.total_added, scope.total_removed), (5, 1));
        assert_eq!(scope.test_files, vec!["tests/common/mod.rs".to_string()]);
        assert_eq!(scope.warnings.len(), 2, "{:?}", scope.warnings);
        assert!(scope.warnings.iter().any(|w| w.contains("test file touched")));
        assert!(scope.warnings.iter().any(|w| w.contains("tests/common/mod.rs: not named by the RCA")));
        // Culprits given with a crate prefix still match; no culprits = no culprit warnings.
        assert!(!scope_from_diff(SAMPLE_DIFF, &["crates/foo/src/parser.rs".into(), "tests/common/mod.rs".into()])
            .warnings
            .iter()
            .any(|w| w.contains("RCA")));
        assert!(!scope_from_diff(SAMPLE_DIFF, &[]).warnings.iter().any(|w| w.contains("RCA")));
        let json = serde_json::to_value(&scope).unwrap();
        assert_eq!(json["files"][0]["added"], 3);
    }

    #[test]
    fn scope_warns_on_large_change() {
        let mut diff = String::from(
            "diff --git a/src/big.rs b/src/big.rs\n--- a/src/big.rs\n+++ b/src/big.rs\n@@ -1,1 +1,101 @@\n",
        );
        for i in 0..101 {
            diff.push_str(&format!("+line {i}\n"));
        }
        let scope = scope_from_diff(&diff, &[]);
        assert_eq!(scope.total_added, 101);
        assert!(scope.warnings.iter().any(|w| w.starts_with("large change: 101 lines")), "{:?}", scope.warnings);
        assert!(scope_from_diff("", &[]).files.is_empty());
    }

    #[test]
    fn path_helpers() {
        assert_eq!(norm_path(".\\tests\\issue_1.rs"), "tests/issue_1.rs");
        assert_eq!(norm_path("/src/x.rs/"), "src/x.rs");
        assert_eq!(issue_number_in_path("crates/a/tests/issue_42.rs"), Some(42));
        assert_eq!(issue_number_in_path("src/p/issue_7_tests.rs"), Some(7));
        assert_eq!(issue_number_in_path("issue_1/src/lib.rs"), None);
        assert!(is_test_path("crates/a/tests/x.rs"));
        assert!(is_test_path("src/parser/issue_7_tests.rs"));
        assert!(is_test_path("src/foo_test.rs"));
        assert!(!is_test_path("src/testsuite.rs"));
        assert!(!is_test_path("tests"));
    }

    /// End-to-end through git: a real repository exercises the `show_file` readers.
    #[test]
    fn repro_and_fix_guards_against_a_git_repo() {
        use crate::git;
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path();
        git::run(wt, &["init", "-q", "-b", "main"]).unwrap();
        git::run(wt, &["config", "user.email", "t@example.com"]).unwrap();
        git::run(wt, &["config", "user.name", "T"]).unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join("Cargo.toml"), CARGO_OLD).unwrap();
        std::fs::write(wt.join("src/lib.rs"), LIB_OLD).unwrap();
        let base = git::commit_all(wt, "init").unwrap();

        // Repro: new test file + dev-dependency -> allowed.
        std::fs::create_dir_all(wt.join("tests")).unwrap();
        std::fs::write(
            wt.join("tests/issue_123.rs"),
            "#[test]\nfn issue_123_rejects_short_frame() { panic!(\"short frame must be rejected\") }\n",
        )
        .unwrap();
        std::fs::write(wt.join("Cargo.toml"), CARGO_DEV).unwrap();
        let changes = git::changed_files(wt, &base).unwrap();
        assert_eq!(check_repro_changes(&changes, &rec(), wt, &base), Ok(()));

        // Repro: production edit -> rejected, naming the file.
        std::fs::write(wt.join("src/lib.rs"), LIB_OLD.replace("pub fn x() {}", "pub fn x() { 1 }")).unwrap();
        let changes = git::changed_files(wt, &base).unwrap();
        let err = check_repro_changes(&changes, &rec(), wt, &base).unwrap_err();
        assert!(err.contains("M src/lib.rs"), "{err}");
        std::fs::write(wt.join("src/lib.rs"), LIB_OLD).unwrap();

        // Freeze the test (S2 checkpoint).
        let checkpoint = git::commit_all(wt, "test: failing repro for #123").unwrap();

        // Fix: production change only -> ok.
        std::fs::write(wt.join("src/lib.rs"), LIB_OLD.replace("pub fn x() {}", "pub fn x() { fixed() }")).unwrap();
        let changes = git::changed_files(wt, &checkpoint).unwrap();
        assert_eq!(check_fix_changes(&changes, &rec(), wt, &checkpoint), Ok(()));

        // Fix: the test file was touched -> rejected.
        std::fs::write(wt.join("tests/issue_123.rs"), "#[test]\n#[ignore]\nfn issue_123_rejects_short_frame() {}\n")
            .unwrap();
        let changes = git::changed_files(wt, &checkpoint).unwrap();
        let err = check_fix_changes(&changes, &rec(), wt, &checkpoint).unwrap_err();
        assert!(err.contains("tests/issue_123.rs"), "{err}");
    }
}
