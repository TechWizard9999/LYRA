//! Universal guard rules for the repro, fix and verify stages (PLAN §3.1, §3.3, §3.4): the
//! reproduction record, repository-relative path validation, the expected-assertion match,
//! the byte-for-byte lock on the frozen test file, and change-scope accounting.
//!
//! Everything toolchain- or runner-specific (which other files a stage may touch, how the
//! test is selected and its output parsed, registration and dependency rules) belongs to
//! the run's execution profile (`crate::profile`, PLAN §13). The two `check_*_changes`
//! entry points add the git / filesystem reads and delegate to the profile.

use crate::profile::ExecutionProfile;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------------------
// Repro record
// ---------------------------------------------------------------------------------------

/// `20-repro/repro.json`: what the agent recorded about the failing test, plus what the
/// harness froze (`frozen_commit`, `frozen_len`, `profile`). Matches
/// `schemas/repro.schema.json` plus the harness-owned fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReproRecord {
    pub reproduced: bool,
    /// The project the test belongs to. For the `rust-cargo` profile this is the Cargo
    /// package name (`cargo test -p <crate>`); the on-disk key stays `crate`.
    #[serde(rename = "crate", default)]
    pub krate: String,
    /// Repository-relative path, forward slashes.
    #[serde(default)]
    pub test_file: String,
    /// Exact test identity as the runner prints it.
    #[serde(default)]
    pub test_name: String,
    /// The profile's regression command for display (rebuilt by the harness, never the agent's).
    #[serde(default)]
    pub command: String,
    /// Assertion message / panic text that proves the bug; must appear in the output.
    #[serde(default)]
    pub expected: String,
    #[serde(default)]
    pub observed: String,
    /// Source file that received the test's registration line, if the profile uses one.
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
    /// Execution profile id that produced and validated this record. Set by the harness;
    /// absent in records written before profiles existed (all of them `rust-cargo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Every other file the repro stage created (fixtures, package initializers), frozen with
    /// the test through Fix and Verify. Set by the harness from the S2 diff; empty in records
    /// written before this field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frozen_inputs: Vec<String>,
}

fn validate_repro_path(path: &str) -> Result<(), String> {
    let path = path.replace('\\', "/");
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(':')
        || path.chars().any(char::is_control)
        || path.split('/').any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("reproduction paths must be repository-relative paths without traversal".into());
    }
    Ok(())
}

/// Universal checks (paths, a prepared test with its expected text) plus the profile's.
pub fn validate_repro_record(rec: &ReproRecord, profile: &dyn ExecutionProfile) -> Result<(), String> {
    validate_repro_path(&rec.test_file)?;
    if let Some(path) = &rec.registration_file {
        validate_repro_path(path)?;
    }
    if !rec.reproduced || rec.expected.trim().is_empty() {
        return Err("reproduction record must describe a prepared test and its expected assertion".into());
    }
    profile.validate_record(rec)
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

pub(crate) fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// True when a directory component (not the file name) equals `name`.
pub(crate) fn has_dir(path: &str, name: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    parts.len() > 1 && parts[..parts.len() - 1].contains(&name)
}

pub(crate) fn is_new(status: &str) -> bool {
    matches!(status, "??" | "A")
}

/// `issue_<n>` in a file name (`tests/issue_123.rs`, `src/p/issue_123_tests.rs`).
pub fn issue_number_in_path(path: &str) -> Option<u64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"issue_(\d+)").unwrap());
    re.captures(base_name(path)).and_then(|c| c[1].parse().ok())
}

// ---------------------------------------------------------------------------------------
// Repro diff rules (PLAN §3.1)
// ---------------------------------------------------------------------------------------

/// S2: the recorded paths resolve inside the worktree without symlinks, then the profile
/// checks the diff against its allowed set. `changes` is `git::changed_files(worktree, base)`.
pub fn check_repro_changes(
    changes: &[(String, String)],
    rec: &ReproRecord,
    worktree: &Path,
    base_commit: &str,
    profile: &dyn ExecutionProfile,
) -> Result<(), String> {
    validate_repro_record(rec, profile)?;
    let root = worktree.canonicalize().map_err(|e| e.to_string())?;
    for path in std::iter::once(rec.test_file.as_str()).chain(rec.registration_file.as_deref()) {
        let mut current = root.clone();
        for component in path.replace('\\', "/").split('/') {
            current.push(component);
            if std::fs::symlink_metadata(&current).map_err(|e| e.to_string())?.file_type().is_symlink() {
                return Err(format!("{path}: reproduction paths must not contain symlinks"));
            }
        }
        if !current.canonicalize().map_err(|e| e.to_string())?.starts_with(&root) {
            return Err(format!("{path}: reproduction path escapes the worktree"));
        }
    }
    let read_old = |p: &str| crate::git::show_file(worktree, base_commit, p).ok();
    let read_new = |p: &str| std::fs::read_to_string(worktree.join(p)).ok();
    profile.check_repro_changes(changes, rec, &read_old, &read_new)
}

// ---------------------------------------------------------------------------------------
// Fix rules (PLAN §3.3): the test is locked
// ---------------------------------------------------------------------------------------

/// S4/S5: the profile's test lock. `changes` is `git::changed_files(worktree, checkpoint)`
/// where `checkpoint` is the S2 commit.
pub fn check_fix_changes(
    changes: &[(String, String)],
    rec: &ReproRecord,
    worktree: &Path,
    checkpoint: &str,
    profile: &dyn ExecutionProfile,
) -> Result<(), String> {
    let read_checkpoint = checkpoint_reader(worktree, checkpoint);
    let read_now = |p: &str| std::fs::read_to_string(worktree.join(p)).ok();
    profile.check_fix_changes(changes, rec, &read_checkpoint, &read_now)
}

/// Reads files at `checkpoint`. Profiles probe many candidate paths (every place an import
/// could resolve), so the commit's file list is taken once with `git ls-tree` and a path that
/// is not in it returns `None` without starting a process; contents are cached. If the
/// listing fails, every read falls back to `git show`.
fn checkpoint_reader<'a>(worktree: &'a Path, checkpoint: &'a str) -> impl Fn(&str) -> Option<String> + 'a {
    let listing: Option<std::collections::HashSet<String>> =
        crate::git::run(worktree, &["ls-tree", "-r", "-z", "--name-only", checkpoint])
            .ok()
            .map(|out| out.split('\0').filter(|p| !p.is_empty()).map(norm_path).collect());
    let cache = std::cell::RefCell::new(std::collections::HashMap::<String, Option<String>>::new());
    move |p: &str| {
        // `dir/` asks whether a directory exists at the checkpoint (`Some("")`).
        if let Some(dir) = p.strip_suffix('/') {
            let prefix = format!("{}/", norm_path(dir));
            return match &listing {
                Some(files) => files.iter().any(|f| f.starts_with(&prefix)).then(String::new),
                None => crate::git::run(worktree, &["ls-tree", "-d", checkpoint, norm_path(dir).as_str()])
                    .ok()
                    .filter(|out| !out.trim().is_empty())
                    .map(|_| String::new()),
            };
        }
        let path = norm_path(p);
        if listing.as_ref().is_some_and(|files| !files.contains(&path)) {
            return None;
        }
        if let Some(hit) = cache.borrow().get(&path) {
            return hit.clone();
        }
        let content = crate::git::show_file(worktree, checkpoint, &path).ok();
        cache.borrow_mut().insert(path, content.clone());
        content
    }
}

fn normalise_text(s: &str) -> String {
    s.replace("\r\n", "\n").trim_end().to_string()
}

/// The universal part of every test lock: the frozen test file and every input the repro
/// stage created with it (`frozen_inputs`: fixtures, package initializers) are byte-for-byte
/// their S2 form (git's own comparison first, then the content after newline normalisation,
/// because `git show` trims and autocrlf may differ). A fix cannot make the locked test pass
/// by changing the data it reads. Returns the violations for the profile to extend.
pub fn frozen_test_violations(
    changes: &[(String, String)],
    rec: &ReproRecord,
    read_checkpoint: &dyn Fn(&str) -> Option<String>,
    read_now: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut violations = Vec::new();
    let frozen = std::iter::once((norm_path(&rec.test_file), "the repro test file"))
        .chain(rec.frozen_inputs.iter().map(|p| (norm_path(p), "a repro input")));
    for (path, what) in frozen {
        if path.is_empty() {
            continue;
        }
        if changes.iter().any(|(_, p)| norm_path(p) == path) {
            violations.push(format!("{path}: {what} was modified, replaced or removed (it is locked)"));
            continue;
        }
        match (read_checkpoint(&path), read_now(&path)) {
            (Some(frozen), Some(now)) if normalise_text(&frozen) != normalise_text(&now) => {
                violations.push(format!("{path}: content of {what} differs from its frozen form"));
            }
            (Some(_), None) => violations.push(format!("{path}: {what} was deleted")),
            _ => {}
        }
    }
    violations
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
/// the RCA did not name (`culprits`, repository-relative; empty = no check) and test files
/// touched (`is_test` is the profile's test-path rule).
pub fn scope_from_diff(diff: &str, culprits: &[String], is_test: &dyn Fn(&str) -> bool) -> Scope {
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
        if is_test(&f.path) {
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
    use crate::profile::rust_cargo::{self, RustCargo};

    const NAME: &str = "issue_123_rejects_short_frame";

    fn rec() -> ReproRecord {
        ReproRecord {
            reproduced: true,
            krate: "foo".into(),
            test_file: "tests/issue_123.rs".into(),
            test_name: NAME.into(),
            command: rust_cargo::repro_test_command("foo", NAME),
            expected: "short frame must be rejected".into(),
            observed: String::new(),
            registration_file: None,
            notes: String::new(),
            frozen_commit: None,
            frozen_len: None,
            profile: None,
            frozen_inputs: Vec::new(),
        }
    }

    #[test]
    fn expected_text_matches_loosely() {
        let out = "thread 'x' panicked at tests/issue_123.rs:9:5:\nassertion `left == right` failed: short frame must be rejected\n  left: Ok(Frame { len: 2 })\n right: Err(TooShort)\n";
        assert!(output_contains_expected(out, "short frame must be rejected"));
        assert!(output_contains_expected(out, "  SHORT frame   must be rejected "));
        assert!(output_contains_expected(out, "left: Ok(Frame { len: 2 })\n right: Err(TooShort)"));
        assert!(!output_contains_expected(out, "something else entirely"));
        assert!(!output_contains_expected(out, "   "));
    }

    #[test]
    fn record_validation_is_universal_plus_profile() {
        assert_eq!(validate_repro_record(&rec(), &RustCargo), Ok(()));
        for path in ["../tests/a.rs", "/tests/a.rs", "C:\\tests\\a.rs", "tests/../../a.rs"] {
            let mut record = rec();
            record.test_file = path.into();
            assert!(validate_repro_record(&record, &RustCargo).is_err(), "{path}");
        }
        let mut record = rec();
        record.registration_file = Some("../lib.rs".into());
        assert!(validate_repro_record(&record, &RustCargo).is_err());
        let mut record = rec();
        record.expected = "  ".into();
        assert!(validate_repro_record(&record, &RustCargo).unwrap_err().contains("expected assertion"));
        let mut record = rec();
        record.reproduced = false;
        assert!(validate_repro_record(&record, &RustCargo).is_err());
        // The profile's own rule is reached after the universal ones.
        let mut record = rec();
        record.krate = "--workspace".into();
        assert!(validate_repro_record(&record, &RustCargo).unwrap_err().contains("package name"));
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
        assert_eq!(rec.profile, None, "records written before profiles existed have none");
        let text = serde_json::to_string(&rec).unwrap();
        assert!(text.contains("\"crate\":\"foo\""));
        assert!(!text.contains("frozen_len"));
        assert!(!text.contains("profile"));
        let back: ReproRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(back, rec);
        let mut tagged = rec.clone();
        tagged.profile = Some("rust-cargo".into());
        let text = serde_json::to_string(&tagged).unwrap();
        assert!(text.contains("\"profile\":\"rust-cargo\""));
        assert_eq!(serde_json::from_str::<ReproRecord>(&text).unwrap(), tagged);
        // `reproduced` is the one field that must be present.
        assert!(serde_json::from_str::<ReproRecord>(r#"{"crate":"foo"}"#).is_err());
        let minimal: ReproRecord = serde_json::from_str(r#"{"reproduced":false}"#).unwrap();
        assert!(!minimal.reproduced);
        assert_eq!(minimal.test_file, "");
    }

    #[test]
    fn frozen_test_lock_is_universal() {
        let frozen =
            "#[test]\nfn issue_123_rejects_short_frame() { assert!(false, \"short frame must be rejected\"); }\n";
        let same = |_: &str| Some(frozen.to_string());
        assert!(frozen_test_violations(&[("M".into(), "src/parser.rs".into())], &rec(), &same, &same).is_empty());
        let listed = frozen_test_violations(&[("M".into(), "tests/issue_123.rs".into())], &rec(), &same, &same);
        assert!(listed[0].contains("locked"), "{listed:?}");
        let drifted = |_: &str| Some(frozen.replace("#[test]", "#[test]\n#[ignore]"));
        let v = frozen_test_violations(&[], &rec(), &same, &drifted);
        assert!(v[0].contains("frozen form"), "{v:?}");
        let crlf = |_: &str| Some(format!("{}\r\n", frozen.replace('\n', "\r\n")));
        assert!(frozen_test_violations(&[], &rec(), &same, &crlf).is_empty());
        let gone = |_: &str| None;
        assert!(frozen_test_violations(&[], &rec(), &same, &gone)[0].contains("deleted"));
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
        let is_test = &rust_cargo::is_test_path;
        let scope = scope_from_diff(SAMPLE_DIFF, &["src/parser.rs".to_string()], is_test);
        assert_eq!(scope.files.len(), 2);
        assert_eq!(scope.files[0], ScopeFile { path: "src/parser.rs".into(), added: 3, removed: 1 });
        assert_eq!(scope.files[1], ScopeFile { path: "tests/common/mod.rs".into(), added: 2, removed: 0 });
        assert_eq!((scope.total_added, scope.total_removed), (5, 1));
        assert_eq!(scope.test_files, vec!["tests/common/mod.rs".to_string()]);
        assert_eq!(scope.warnings.len(), 2, "{:?}", scope.warnings);
        assert!(scope.warnings.iter().any(|w| w.contains("test file touched")));
        assert!(scope.warnings.iter().any(|w| w.contains("tests/common/mod.rs: not named by the RCA")));
        // Culprits given with a crate prefix still match; no culprits = no culprit warnings.
        assert!(!scope_from_diff(
            SAMPLE_DIFF,
            &["crates/foo/src/parser.rs".into(), "tests/common/mod.rs".into()],
            is_test
        )
        .warnings
        .iter()
        .any(|w| w.contains("RCA")));
        assert!(!scope_from_diff(SAMPLE_DIFF, &[], is_test).warnings.iter().any(|w| w.contains("RCA")));
        // The test-path rule comes from the profile.
        assert!(scope_from_diff(SAMPLE_DIFF, &[], &|_| false).test_files.is_empty());
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
        let scope = scope_from_diff(&diff, &[], &|_| false);
        assert_eq!(scope.total_added, 101);
        assert!(scope.warnings.iter().any(|w| w.starts_with("large change: 101 lines")), "{:?}", scope.warnings);
        assert!(scope_from_diff("", &[], &|_| false).files.is_empty());
    }

    #[test]
    fn path_helpers() {
        assert_eq!(norm_path(".\\tests\\issue_1.rs"), "tests/issue_1.rs");
        assert_eq!(norm_path("/src/x.rs/"), "src/x.rs");
        assert_eq!(issue_number_in_path("crates/a/tests/issue_42.rs"), Some(42));
        assert_eq!(issue_number_in_path("src/p/issue_7_tests.rs"), Some(7));
        assert_eq!(issue_number_in_path("issue_1/src/lib.rs"), None);
        assert_eq!(base_name("a/b/c.rs"), "c.rs");
        assert!(has_dir("a/tests/x.rs", "tests"));
        assert!(!has_dir("tests", "tests"));
        assert!(is_new("??") && is_new("A") && !is_new("M"));
    }

    /// End-to-end through git: a real repository exercises the `show_file` readers.
    #[test]
    fn repro_and_fix_guards_against_a_git_repo() {
        use crate::git;
        const CARGO_OLD: &str = "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n";
        const CARGO_DEV: &str = "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n\n[dev-dependencies]\nproptest = \"1\"\n";
        const LIB_OLD: &str = "pub mod parser;\n\npub fn x() {}\n";
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
        assert_eq!(check_repro_changes(&changes, &rec(), wt, &base, &RustCargo), Ok(()));

        // Repro: production edit -> rejected, naming the file.
        std::fs::write(wt.join("src/lib.rs"), LIB_OLD.replace("pub fn x() {}", "pub fn x() { 1 }")).unwrap();
        let changes = git::changed_files(wt, &base).unwrap();
        let err = check_repro_changes(&changes, &rec(), wt, &base, &RustCargo).unwrap_err();
        assert!(err.contains("M src/lib.rs"), "{err}");
        std::fs::write(wt.join("src/lib.rs"), LIB_OLD).unwrap();

        // Freeze the test (S2 checkpoint).
        let checkpoint = git::commit_all(wt, "test: failing repro for #123").unwrap();

        // Fix: production change only -> ok.
        std::fs::write(wt.join("src/lib.rs"), LIB_OLD.replace("pub fn x() {}", "pub fn x() { fixed() }")).unwrap();
        let changes = git::changed_files(wt, &checkpoint).unwrap();
        assert_eq!(check_fix_changes(&changes, &rec(), wt, &checkpoint, &RustCargo), Ok(()));

        // Fix: the test file was touched -> rejected.
        std::fs::write(wt.join("tests/issue_123.rs"), "#[test]\n#[ignore]\nfn issue_123_rejects_short_frame() {}\n")
            .unwrap();
        let changes = git::changed_files(wt, &checkpoint).unwrap();
        let err = check_fix_changes(&changes, &rec(), wt, &checkpoint, &RustCargo).unwrap_err();
        assert!(err.contains("tests/issue_123.rs"), "{err}");
    }

    /// The checkpoint reader answers from the commit, not the worktree: listed files are read
    /// (and cached), unlisted ones are `None` without a `git show`, and later worktree edits do
    /// not leak in.
    #[test]
    fn checkpoint_reader_reads_the_commit_and_skips_unlisted_paths() {
        use crate::git;
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path();
        git::run(wt, &["init", "-q", "-b", "main"]).unwrap();
        git::run(wt, &["config", "user.email", "t@example.com"]).unwrap();
        git::run(wt, &["config", "user.name", "T"]).unwrap();
        std::fs::create_dir_all(wt.join("pkg")).unwrap();
        std::fs::write(wt.join("pkg/core.py"), "x = 1\n").unwrap();
        let sha = git::commit_all(wt, "init").unwrap();
        std::fs::write(wt.join("pkg/core.py"), "x = 2\n").unwrap();
        std::fs::write(wt.join("pkg/new.py"), "y = 1\n").unwrap();
        let read = checkpoint_reader(wt, &sha);
        assert_eq!(read("pkg/core.py").as_deref().map(str::trim), Some("x = 1"));
        assert_eq!(read("pkg\\core.py").as_deref().map(str::trim), Some("x = 1"), "normalised path, cached");
        assert_eq!(read("pkg/new.py"), None, "not in the commit");
        assert_eq!(read("pkg/missing.py"), None);
        assert_eq!(read("pkg/").as_deref(), Some(""), "directory query");
        assert_eq!(read("other/"), None);
        let started = std::time::Instant::now();
        for i in 0..2000 {
            assert_eq!(read(&format!("pkg/probe_{i}.py")), None);
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "unlisted probes start no process");
    }

    /// A fix must not make the locked test pass by changing the data it reads: every file the
    /// repro stage created is frozen with the test, for every profile.
    #[test]
    fn repro_created_fixtures_are_locked_through_fix_for_every_profile() {
        use crate::git;
        use crate::profile::{python_pytest::PythonPytest, ExecutionProfile};
        let cases: [(&dyn ExecutionProfile, &str, &str, &str); 2] = [
            (&RustCargo, "Cargo.toml", "tests/issue_123.rs", "tests/fixtures/frame.bin"),
            (&PythonPytest, "pytest.ini", "tests/test_issue_123.py", "tests/fixtures/input.json"),
        ];
        for (profile, manifest, test_file, fixture) in cases {
            let dir = tempfile::tempdir().unwrap();
            let wt = dir.path();
            // A production source file of the profile's language.
            let source = if profile.id() == "python-pytest" { "src/core.py" } else { "src/core.rs" };
            git::run(wt, &["init", "-q", "-b", "main"]).unwrap();
            git::run(wt, &["config", "user.email", "t@example.com"]).unwrap();
            git::run(wt, &["config", "user.name", "T"]).unwrap();
            std::fs::write(wt.join(manifest), "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n").unwrap();
            std::fs::create_dir_all(wt.join("src")).unwrap();
            std::fs::write(wt.join(source), "buggy\n").unwrap();
            if profile.id() == "python-pytest" {
                // Production code is what the repository declares.
                std::fs::write(wt.join("pyproject.toml"), "[tool.setuptools]\npy-modules = [\"core\"]\n").unwrap();
            }
            git::commit_all(wt, "init").unwrap();

            // S2: the test plus a fixture holding the input that triggers the bug.
            std::fs::create_dir_all(wt.join("tests/fixtures")).unwrap();
            std::fs::write(wt.join(test_file), "reads tests/fixtures and fails on the buggy input\n").unwrap();
            std::fs::write(wt.join(fixture), "{\"frame\": [1, 2]}\n").unwrap();
            let mut record = rec();
            record.test_file = test_file.into();
            record.test_name = if profile.id() == "python-pytest" {
                format!("{test_file}::test_issue_123_rejects_short_frame")
            } else {
                "issue_123_rejects_short_frame".into()
            };
            record.command = profile.regression_command_line(&record);
            record.frozen_inputs = vec![fixture.into()];
            let checkpoint = git::commit_all(wt, "test: failing repro for #123").unwrap();

            // A real production fix passes the lock.
            std::fs::write(wt.join(source), "fixed\n").unwrap();
            let changes = git::changed_files(wt, &checkpoint).unwrap();
            assert_eq!(check_fix_changes(&changes, &record, wt, &checkpoint, profile), Ok(()), "{}", profile.id());

            // Changing only the fixture to a non-triggering input is rejected...
            std::fs::write(wt.join(source), "buggy\n").unwrap();
            std::fs::write(wt.join(fixture), "{\"frame\": [1, 2, 3, 4]}\n").unwrap();
            let changes = git::changed_files(wt, &checkpoint).unwrap();
            let err = check_fix_changes(&changes, &record, wt, &checkpoint, profile).unwrap_err();
            assert!(err.contains(fixture) && err.contains("repro input"), "{}: {err}", profile.id());
            // ...as is deleting it...
            std::fs::remove_file(wt.join(fixture)).unwrap();
            let changes = git::changed_files(wt, &checkpoint).unwrap();
            assert!(check_fix_changes(&changes, &record, wt, &checkpoint, profile).unwrap_err().contains(fixture));
            // ...or replacing it, even alongside a production change.
            std::fs::write(wt.join(fixture), "[]\n").unwrap();
            std::fs::write(wt.join(source), "fixed\n").unwrap();
            let changes = git::changed_files(wt, &checkpoint).unwrap();
            assert!(check_fix_changes(&changes, &record, wt, &checkpoint, profile).unwrap_err().contains(fixture));
        }
    }

    #[test]
    fn frozen_inputs_round_trip_and_old_records_load_without_them() {
        let mut record = rec();
        record.frozen_inputs = vec!["tests/fixtures/a.json".into()];
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["frozen_inputs"], serde_json::json!(["tests/fixtures/a.json"]));
        let mut old = json.clone();
        old.as_object_mut().unwrap().remove("frozen_inputs");
        let loaded: ReproRecord = serde_json::from_value(old).unwrap();
        assert!(loaded.frozen_inputs.is_empty());
        assert!(serde_json::to_value(&loaded).unwrap().get("frozen_inputs").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn repro_paths_through_symlinks_are_rejected() {
        use crate::git;
        let outer = tempfile::tempdir().unwrap();
        let wt = outer.path().join("wt");
        std::fs::create_dir_all(wt.join("tests")).unwrap();
        git::run(&wt, &["init", "-q", "-b", "main"]).unwrap();
        git::run(&wt, &["config", "user.email", "t@example.com"]).unwrap();
        git::run(&wt, &["config", "user.name", "T"]).unwrap();
        std::fs::write(wt.join("Cargo.toml"), "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n").unwrap();
        let base = git::commit_all(&wt, "init").unwrap();
        std::fs::write(outer.path().join("outside.rs"), "#[test] fn issue_123_rejects_short_frame() {}").unwrap();
        std::os::unix::fs::symlink(outer.path().join("outside.rs"), wt.join("tests/issue_123.rs")).unwrap();
        let changes = git::changed_files(&wt, &base).unwrap();
        let err = check_repro_changes(&changes, &rec(), &wt, &base, &RustCargo).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
    }
}
