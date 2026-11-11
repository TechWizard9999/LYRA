//! The `rust-cargo` execution profile: Cargo workspaces, libtest output, `rustfmt`,
//! `Cargo.toml` / `Cargo.lock` rules and the `#[cfg(test)] mod issue_<n>_tests;`
//! registration convention. This is the only built-in profile; everything Cargo-specific
//! that used to live in the stages and guards lives here (PLAN §13).

use super::{Discovery, ExecutionProfile, FileReader, Preparation, RegressionOutcome, ResolvedProfile, VerifyStep};
use crate::config::{VerifyCommands, VerifyConfig};
use crate::guards::{self, ReproRecord};
use crate::proc::{self, CommandOutput, CommandSpec};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const ID: &str = "rust-cargo";
/// What `detect` looks for; quoted in the unsupported-repository diagnostic.
pub const DETECTION: &str = "Cargo.toml at the repository root";

pub struct RustCargo;

/// `Cargo.toml` at the worktree root. Nothing is executed.
pub fn detect(worktree: &Path) -> Option<ResolvedProfile> {
    worktree.join("Cargo.toml").is_file().then(|| ResolvedProfile {
        id: ID.into(),
        version: super::CONTRACT_VERSION,
        project_dir: String::new(),
        evidence: vec!["Cargo.toml".into()],
    })
}

#[async_trait]
impl ExecutionProfile for RustCargo {
    fn id(&self) -> &'static str {
        ID
    }
    fn detect(&self, worktree: &Path) -> Option<ResolvedProfile> {
        detect(worktree)
    }
    fn detection(&self) -> &'static str {
        DETECTION
    }
    fn required_tools(&self) -> &'static [&'static str] {
        &["cargo", "rustfmt"]
    }
    fn is_toolchain_output(&self, path: &str) -> bool {
        crate::guards::base_name(path) == "Cargo.lock"
    }
    fn language(&self) -> &'static str {
        "Rust"
    }
    fn prompt_macros(&self) -> &'static str {
        include_str!("../prompts/profiles/rust-cargo.md")
    }

    fn is_source_file(&self, path: &Path) -> bool {
        path.extension().is_some_and(|s| s == "rs") || path.file_name().is_some_and(|n| n == "Cargo.toml")
    }
    fn source_extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }
    fn manifest_names(&self) -> &'static [&'static str] {
        &["Cargo.toml"]
    }
    fn generated_dirs(&self) -> &'static [&'static str] {
        &["target", "node_modules"]
    }
    fn is_test_path(&self, path: &str) -> bool {
        is_test_path(path)
    }
    async fn describe_repository(&self, worktree: &Path, timeout: Duration, cancel: &CancellationToken) -> Value {
        let spec =
            CommandSpec::new("cargo", worktree).args(["metadata", "--format-version", "1", "--no-deps"]).scrubbed();
        match proc::run(&spec, timeout, cancel).await {
            Ok(out) if out.success() => parse_cargo_metadata(&out.stdout),
            Ok(out) => json!({ "error": cap(&out.combined(), 4000) }),
            Err(e) => json!({ "error": format!("{e:#}") }),
        }
    }

    fn validate_record(&self, rec: &ReproRecord) -> Result<(), String> {
        repro_test_args(&rec.krate, &rec.test_name)?;
        if !rec.test_file.ends_with(".rs") {
            return Err("the reproduction test must be a Rust source file".into());
        }
        Ok(())
    }
    fn regression_command_line(&self, rec: &ReproRecord, _worktree: &Path) -> String {
        repro_test_command(&rec.krate, &rec.test_name)
    }
    fn regression_command(&self, rec: &ReproRecord, cwd: &Path) -> Result<CommandSpec, String> {
        Ok(CommandSpec::new("cargo", cwd).args(repro_test_args(&rec.krate, &rec.test_name)?))
    }
    fn discovery(&self, spec: &CommandSpec) -> Discovery {
        Discovery {
            spec: source_binding_spec(spec).arg("--list"),
            note: "The discovery-only lint cap preserves active environment flags, or appends build.rustflags. Target-specific Cargo rustflags may take precedence; discovery then fails closed. Real test commands retain their original flags.",
        }
    }
    fn format_test(&self, worktree: &Path, test_file: &Path) -> Option<CommandSpec> {
        Some(
            CommandSpec::new("rustfmt", worktree)
                .args(["--edition", "2021"])
                .arg(test_file.to_string_lossy())
                .scrubbed(),
        )
    }
    fn classify(&self, output: &str, rec: &ReproRecord) -> RegressionOutcome {
        classify_cargo_test(output, &rec.test_name)
    }
    fn describe_outcome(&self, outcome: &RegressionOutcome, rec: &ReproRecord) -> String {
        describe_test_run(outcome, &rec.test_name)
    }
    fn confirm_failure(&self, run: &CommandOutput, _rec: &ReproRecord) -> Result<(), String> {
        let summaries = parse_summaries(&run.combined());
        if run.exit_code != Some(101)
            || summaries.iter().map(|s| s.failed).sum::<u64>() != 1
            || summaries.iter().any(|s| s.passed != 0 || s.ignored != 0)
        {
            return Err(
                "reproduction must execute exactly one failing test and exit with Cargo's test-failure status".into()
            );
        }
        Ok(())
    }
    fn assert_passed(&self, output: &str, rec: &ReproRecord) -> Result<(), String> {
        assert_test_passed(output, &rec.test_name)
    }
    fn assert_absent_from_discovery(&self, output: &str, rec: &ReproRecord) -> Result<(), String> {
        assert_test_absent_from_list(output, &rec.test_name)
    }

    fn check_repro_changes(
        &self,
        changes: &[(String, String)],
        rec: &ReproRecord,
        read_old: FileReader<'_>,
        read_new: FileReader<'_>,
    ) -> Result<(), String> {
        check_repro_changes_with(changes, rec, read_old, read_new)
    }
    fn check_fix_changes(
        &self,
        changes: &[(String, String)],
        rec: &ReproRecord,
        read_checkpoint: FileReader<'_>,
        read_now: FileReader<'_>,
        _checkpoint_paths: Option<&std::collections::BTreeSet<String>>,
    ) -> Result<(), String> {
        check_fix_changes_with(changes, rec, read_checkpoint, read_now)
    }

    fn reproduction_environment(&self, spec: CommandSpec, run_root: &Path) -> CommandSpec {
        spec.scrubbed().env("CARGO_TARGET_DIR", run_root.join("repro-target").to_string_lossy())
    }
    fn check_environment_root(&self, checkout: &Path) -> Result<(), String> {
        ensure_no_ancestor_cargo_config(checkout)
    }
    fn create_environment(&self, env_root: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(cargo_home(env_root))
    }
    fn check_environment(&self, env_root: &Path) -> Result<(), String> {
        ensure_empty_cargo_config(&cargo_home(env_root))
    }
    fn command_environment(&self, spec: CommandSpec, env_root: &Path) -> CommandSpec {
        controlled_cargo_environment(spec, &cargo_home(env_root), &target_dir(env_root))
    }
    fn preparation(&self, worktree: &Path, present: &dyn Fn(&Path) -> bool) -> Option<Preparation> {
        // Library repositories may intentionally omit Cargo.lock. Generate that one Cargo
        // output before the baseline is frozen; afterwards it is protected like every file.
        if present(Path::new("Cargo.lock")) {
            return None;
        }
        Some(Preparation {
            name: "lockfile",
            spec: CommandSpec::new("cargo", worktree).arg("generate-lockfile"),
            log: "50-verify/lockfile.txt",
            creates: vec![PathBuf::from("Cargo.lock")],
        })
    }
    fn verification_steps(
        &self,
        verify: &VerifyCommands,
        rec: &ReproRecord,
        worktree: &Path,
    ) -> Result<Vec<VerifyStep>, String> {
        // `None` falls back to the historical defaults, which the operator's top-level
        // `[verify]` table normally supplies already (Config::verify_commands).
        let defaults = VerifyConfig::default();
        let verify = VerifyConfig {
            build: verify.build.clone().unwrap_or(defaults.build),
            test: verify.test.clone().unwrap_or(defaults.test),
            fmt: verify.fmt.clone().unwrap_or(defaults.fmt),
        };
        let line = |name: &str, line: &str| {
            CommandSpec::from_line(line, worktree).map_err(|_| format!("verify step '{name}' has an empty command"))
        };
        Ok(vec![
            VerifyStep {
                name: "build",
                spec: line("build", &verify.build)?,
                log: "50-verify/build.txt",
                regression: false,
            },
            VerifyStep {
                name: "repro",
                spec: self.regression_command(rec, worktree)?,
                log: "50-verify/repro-after.txt",
                regression: true,
            },
            VerifyStep {
                name: "tests",
                spec: line("tests", &verify.test)?,
                log: "50-verify/tests.txt",
                regression: false,
            },
            VerifyStep { name: "fmt", spec: line("fmt", &verify.fmt)?, log: "50-verify/fmt.txt", regression: false },
        ])
    }
    fn explain_failure(&self, output: &str, exit_code: Option<i32>) -> String {
        first_error_line(output, exit_code)
    }
}

fn cargo_home(env_root: &Path) -> PathBuf {
    env_root.join("cargo-home")
}
fn target_dir(env_root: &Path) -> PathBuf {
    env_root.join("target")
}

fn cap(s: &str, max: usize) -> String {
    crate::stages::context::cap(s, max)
}

// ---------------------------------------------------------------------------------------
// Regression command
// ---------------------------------------------------------------------------------------

/// The one command the harness runs for the repro test, in S2 and again in S5.
pub fn repro_test_command(krate: &str, test_name: &str) -> String {
    format!("cargo test -p {} {} -- --exact", krate.trim(), test_name.trim())
}

/// Validate agent-provided selectors before they become separate process arguments.
pub fn repro_test_args(krate: &str, test_name: &str) -> Result<Vec<String>, String> {
    let package = Regex::new(r"^[A-Za-z0-9_][A-Za-z0-9_-]*$").expect("package regex");
    let test = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*$").expect("test regex");
    if !package.is_match(krate) {
        return Err("invalid Cargo package name in reproduction record".into());
    }
    if !test.is_match(test_name) {
        return Err("invalid exact test name in reproduction record".into());
    }
    Ok(["test", "-p", krate, test_name, "--", "--exact"].into_iter().map(str::to_string).collect())
}

/// Emptying a test module can make existing test-only helpers/imports unused.
/// Cap lints only for discovery, preserving the active rustflags source. Cargo
/// merges the fallback array with build.rustflags; target-specific rustflags
/// still take precedence, so we fail closed rather than replace their cfg flags.
fn source_binding_spec(original: &CommandSpec) -> CommandSpec {
    let mut spec = original.clone();
    let active = |key: &str| {
        original
            .env
            .iter()
            .rev()
            .find(|(name, _)| name == key || (cfg!(windows) && name.eq_ignore_ascii_case(key)))
            .map(|(_, value)| value.clone())
            .or_else(|| (!original.clear_env).then(|| std::env::var(key).ok()).flatten())
    };
    if let Some(mut encoded) = active("CARGO_ENCODED_RUSTFLAGS") {
        if !encoded.is_empty() {
            encoded.push('\u{1f}');
        }
        encoded.push_str("--cap-lints=allow");
        spec.env.push(("CARGO_ENCODED_RUSTFLAGS".into(), encoded));
    } else if let Some(mut flags) = active("RUSTFLAGS") {
        if !flags.is_empty() {
            flags.push(' ');
        }
        flags.push_str("--cap-lints=allow");
        spec.env.push(("RUSTFLAGS".into(), flags));
    } else {
        spec.args.splice(0..0, ["--config".into(), "build.rustflags=[\"--cap-lints=allow\"]".into()]);
    }
    spec
}

// ---------------------------------------------------------------------------------------
// cargo test output
// ---------------------------------------------------------------------------------------

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
pub fn classify_cargo_test(output: &str, test_name: &str) -> RegressionOutcome {
    use RegressionOutcome::*;
    let prefix = format!("test {} ... ", test_name.trim());
    let mut statuses: Vec<RegressionOutcome> = Vec::new();
    for line in output.lines().map(str::trim) {
        let Some(rest) = line.strip_prefix(prefix.as_str()) else {
            continue;
        };
        statuses.push(if rest.starts_with("ok") {
            Passed
        } else if rest.starts_with("FAILED") {
            Failed
        } else if rest.starts_with("ignored") {
            Skipped
        } else {
            Other(format!("unrecognised status for test {test_name}: {rest}"))
        });
    }
    if statuses.contains(&Failed) {
        return Failed;
    }
    if let Some(o) = statuses.iter().find(|s| matches!(s, Other(_))) {
        return o.clone();
    }
    if statuses.contains(&Passed) {
        return Passed;
    }
    if statuses.contains(&Skipped) {
        return Skipped;
    }

    let mut first_error: Option<&str> = None;
    for line in output.lines().map(str::trim) {
        if line.starts_with("error[E") || line.starts_with("error: could not compile") {
            return SetupFailed;
        }
        if line.starts_with("error") && !line.starts_with("error: test failed") && first_error.is_none() {
            first_error = Some(line);
        }
    }
    if let Some(e) = first_error {
        return Other(e.to_string());
    }
    let summaries = parse_summaries(output);
    if summaries.is_empty() {
        return Other("no `test result:` summary in the output".to_string());
    }
    if summaries.iter().all(|s| s.passed + s.failed + s.ignored == 0) {
        return NotDiscovered;
    }
    Other(format!("test {test_name} did not appear in the output"))
}

/// Human-readable reason for a non-successful (or unexpected) classification.
pub fn describe_test_run(run: &RegressionOutcome, test_name: &str) -> String {
    match run {
        RegressionOutcome::SetupFailed => "the test did not compile (build error)".to_string(),
        RegressionOutcome::NotDiscovered => format!("test {test_name} did not run (0 tests executed: filtered out or not discovered; check test_name against what cargo prints)"),
        RegressionOutcome::Passed => format!("test {test_name} passed"),
        RegressionOutcome::Failed => format!("test {test_name} FAILED"),
        RegressionOutcome::Skipped => format!("test {test_name} was ignored"),
        RegressionOutcome::Other(s) => s.clone(),
    }
}

/// Verify rule (PLAN §3.4 step 2): the named test passed and exactly one test ran.
pub fn assert_test_passed(output: &str, test_name: &str) -> Result<(), String> {
    match classify_cargo_test(output, test_name) {
        RegressionOutcome::Passed => {
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

/// A negative-control `--exact --list` run must discover no test after the new
/// reproduction source is emptied. This binds the selected test to the frozen file.
pub(crate) fn assert_test_absent_from_list(output: &str, test_name: &str) -> Result<(), String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^(\d+) tests?, (\d+) benchmarks?$").unwrap());
    let mut saw_summary = false;
    for line in output.lines().map(str::trim) {
        if line == format!("{test_name}: test") || line == format!("{test_name}: benchmark") {
            return Err(format!("test {test_name} is still discovered without the recorded reproduction source"));
        }
        if let Some(captures) = re.captures(line) {
            saw_summary = true;
            if captures[1].parse::<u64>() != Ok(0) || captures[2].parse::<u64>() != Ok(0) {
                return Err(format!("tests are still discovered without the recorded reproduction source: {line}"));
            }
        }
    }
    if !saw_summary {
        return Err("source binding check produced no test-discovery summary".into());
    }
    Ok(())
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

// ---------------------------------------------------------------------------------------
// Workspace description (10-context/repo.json)
// ---------------------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------------------

/// A path that cargo or convention treats as test code.
pub fn is_test_path(path: &str) -> bool {
    let name = guards::base_name(path);
    guards::has_dir(path, "tests")
        || guards::has_dir(path, "benches")
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
/// `read_old` = content at the base commit, `read_new` = content in the worktree.
pub fn check_repro_changes_with(
    changes: &[(String, String)],
    rec: &ReproRecord,
    read_old: FileReader<'_>,
    read_new: FileReader<'_>,
) -> Result<(), String> {
    let test_file = guards::norm_path(&rec.test_file);
    let reg_file = rec.registration_file.as_deref().map(guards::norm_path).filter(|s| !s.is_empty());
    let mut violations: Vec<String> = Vec::new();
    let mut saw_test_file = false;

    for (status, raw) in changes {
        let path = guards::norm_path(raw);
        let status = status.as_str();
        if path == test_file {
            saw_test_file = true;
            if !guards::is_new(status) {
                violations.push(format!("{status} {path}: the test file must be a new file, not a modified one"));
            }
            continue;
        }
        let name = guards::base_name(&path);
        if name == "Cargo.lock" {
            let old = read_old(&path);
            let new = read_new(&path);
            if !new.as_deref().is_some_and(|new| cargo_lock_only_additions(old.as_deref(), new)) {
                violations.push(format!(
                    "{status} {path}: existing locked packages may not be removed, downgraded or changed"
                ));
            }
            continue;
        }
        if guards::is_new(status) && (guards::has_dir(&path, "tests") || guards::has_dir(&path, "fixtures")) {
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

/// Adding test dependencies may append packages and workspace dependency edges. Existing
/// registry/git packages and all existing package identities must remain unchanged.
pub fn cargo_lock_only_additions(old: Option<&str>, new: &str) -> bool {
    let Ok(mut new_doc) = new.parse::<toml::Table>() else { return false };
    let Some(new_packages) = new_doc.remove("package").and_then(|v| v.as_array().cloned()) else { return false };
    let Some(old) = old else {
        return new_packages.iter().all(|p| {
            p.get("name").and_then(toml::Value::as_str).is_some()
                && p.get("version").and_then(toml::Value::as_str).is_some()
        });
    };
    let Ok(mut old_doc) = old.parse::<toml::Table>() else { return false };
    let Some(old_packages) = old_doc.remove("package").and_then(|v| v.as_array().cloned()) else { return false };
    if old_doc != new_doc {
        return false;
    }
    for old_package in old_packages {
        let same_identity =
            |p: &&toml::Value| ["name", "version", "source"].iter().all(|key| old_package.get(key) == p.get(key));
        let matches: Vec<_> = new_packages.iter().filter(same_identity).collect();
        if matches.len() != 1 {
            return false;
        }
        let new_package = matches[0];
        if old_package.get("source").is_some() {
            if &old_package != new_package {
                return false;
            }
            continue;
        }
        let (Some(mut old_table), Some(mut new_table)) =
            (old_package.as_table().cloned(), new_package.as_table().cloned())
        else {
            return false;
        };
        let old_deps = old_table.remove("dependencies").unwrap_or_else(|| toml::Value::Array(vec![]));
        let new_deps = new_table.remove("dependencies").unwrap_or_else(|| toml::Value::Array(vec![]));
        let (Some(old_deps), Some(new_deps)) = (old_deps.as_array(), new_deps.as_array()) else { return false };
        if old_table != new_table
            || !old_deps.iter().all(|dep| new_deps.contains(dep))
            || !new_deps.iter().all(|dep| dep.is_str())
        {
            return false;
        }
    }
    true
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
    let mut added = Vec::new();
    for line in new.lines().map(str::trim_end) {
        if i < old.len() && line == old[i] {
            i += 1;
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        if is_registration_line(line) {
            added.push(line.trim());
            continue;
        }
        return false;
    }
    i == old.len() && (added.is_empty() || one_line_re().is_match(&added.join(" ")))
}

/// Extract the declaration and every immediately preceding attribute/comment line. This
/// locks attributes such as #[path] and extra #[cfg], including multiline attributes.
fn registration_block(content: &str, issue: Option<u64>) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let matches: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let number = mod_line_issue(line).or_else(|| one_line_issue(line))?;
            issue.is_none_or(|wanted| wanted == number).then_some(i)
        })
        .collect();
    if matches.len() != 1 {
        return None;
    }
    let end = matches[0];
    let mut start = end;
    let mut in_attribute = false;
    let mut in_comment = false;
    let trailing_comment = Regex::new(r"(?://.*|/\*.*?\*/)\s*$").expect("comment regex");
    while start > 0 {
        let line = lines[start - 1].trim();
        let code = trailing_comment.replace(line, "");
        let code = code.trim_end();
        if in_comment {
            start -= 1;
            if line.contains("/*") {
                in_comment = false;
            }
        } else if in_attribute {
            start -= 1;
            if line.starts_with("#[") {
                in_attribute = false;
            }
        } else if line.is_empty() || line.starts_with("//") || line.starts_with("#[") {
            start -= 1;
        } else if code.ends_with(']') {
            start -= 1;
            in_attribute = !code.starts_with("#[");
        } else if line.ends_with("*/") {
            start -= 1;
            in_comment = !line.contains("/*");
        } else {
            break;
        }
    }
    Some(lines[start..=end].join("\n").trim_start_matches('\n').to_string())
}

fn registration_item(content: &str, issue: Option<u64>) -> Option<syn::ItemMod> {
    let file = syn::parse_file(content).ok()?;
    let mut modules = file.items.into_iter().filter_map(|item| match item {
        syn::Item::Mod(module)
            if mod_line_issue(&format!("mod {};", module.ident))
                .is_some_and(|n| issue.is_none_or(|wanted| wanted == n)) =>
        {
            Some(module)
        }
        _ => None,
    });
    let module = modules.next()?;
    (modules.next().is_none() && module.content.is_none() && module.semi.is_some()).then_some(module)
}

/// Inspect opaque macro bodies as tokens too: an AST visitor alone does not see
/// declarations hidden inside macro_rules! definitions or generated include! calls.
#[derive(Default)]
struct RegistrationTokens {
    modules: Vec<String>,
    includes: Vec<syn::Macro>,
}

impl syn::parse::Parse for RegistrationTokens {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        use syn::parse::discouraged::Speculative;
        let mut audit = Self::default();
        while !input.is_empty() {
            if input.peek(syn::Token![mod]) {
                input.parse::<syn::Token![mod]>()?;
                if input.peek(syn::Ident) {
                    audit.modules.push(input.parse::<syn::Ident>()?.to_string().trim_start_matches("r#").to_string());
                }
                continue;
            }
            if input.peek(syn::Ident) {
                let fork = input.fork();
                if let Ok(mac) = fork.parse::<syn::Macro>() {
                    if mac.path.segments.last().is_some_and(|segment| segment.ident == "include") {
                        input.advance_to(&fork);
                        audit.includes.push(mac);
                        continue;
                    }
                }
                input.parse::<syn::Ident>()?;
                continue;
            }
            let nested;
            if input.peek(syn::token::Brace) {
                syn::braced!(nested in input);
            } else if input.peek(syn::token::Bracket) {
                syn::bracketed!(nested in input);
            } else if input.peek(syn::token::Paren) {
                syn::parenthesized!(nested in input);
            } else {
                input.step(|cursor| {
                    cursor.token_tree().map(|(_, next)| ((), next)).ok_or_else(|| cursor.error("expected Rust token"))
                })?;
                continue;
            }
            let mut inner = nested.parse::<Self>()?;
            audit.modules.append(&mut inner.modules);
            audit.includes.append(&mut inner.includes);
        }
        Ok(audit)
    }
}

fn include_may_load_test(mac: &syn::Macro, source: &str, test_file: &str) -> bool {
    let Ok(syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(path), .. })) = syn::parse2(mac.tokens.clone()) else {
        // Computed include paths cannot establish which source will be compiled.
        return true;
    };
    let path = path.value().replace('\\', "/");
    let joined = format!("{}/{}", source.rsplit_once('/').map(|(parent, _)| parent).unwrap_or(""), path);
    let mut parts = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    parts.join("/") == test_file || path.ends_with(&format!("/{test_file}"))
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

/// Rejects a fix that touched the frozen test (universal rule in `guards`), its registration
/// attribute block, `Cargo.toml` test sections, `.cargo/`, or the recorded command.
/// `read_checkpoint` = content at the S2 commit, `read_now` = content in the worktree.
pub fn check_fix_changes_with(
    changes: &[(String, String)],
    rec: &ReproRecord,
    read_checkpoint: FileReader<'_>,
    read_now: FileReader<'_>,
) -> Result<(), String> {
    if changes.is_empty() {
        return Err("fix produced no changes".to_string());
    }
    let test_file = guards::norm_path(&rec.test_file);
    let issue = guards::issue_number_in_path(&test_file);

    // 1. The test file is byte-for-byte its frozen form.
    let mut violations = guards::frozen_test_violations(changes, rec, read_checkpoint, read_now);

    // 2. The entire registration attribute block is frozen, not merely cfg(test).
    if let Some(reg) = rec.registration_file.as_deref().map(guards::norm_path).filter(|s| !s.is_empty()) {
        let old_source = read_checkpoint(&reg).unwrap_or_default();
        let new_source = read_now(&reg).unwrap_or_default();
        let old = registration_block(&old_source, issue);
        let new = registration_block(&new_source, issue);
        let old_item = registration_item(&old_source, issue);
        let new_item = registration_item(&new_source, issue);
        if old.is_none() || old != new || old_item.is_none() || old_item != new_item {
            violations.push(format!("{reg}: the frozen test registration line or its attribute block was changed"));
        }
    }

    // 3. Per-file rules.
    for (status, raw) in changes {
        let path = guards::norm_path(raw);
        if path.ends_with(".rs") {
            if let Some(current) = read_now(&path) {
                let previous = read_checkpoint(&path).unwrap_or_default();
                let old = syn::parse_str::<RegistrationTokens>(&previous).unwrap_or_default();
                match syn::parse_str::<RegistrationTokens>(&current) {
                    Ok(new) => {
                        let name = issue.map(|n| format!("issue_{n}_tests"));
                        if name.as_ref().is_some_and(|name| {
                            new.modules.iter().filter(|m| *m == name).count()
                                > old.modules.iter().filter(|m| *m == name).count()
                        }) {
                            violations.push(format!("{path}: another declaration of the frozen test module was added"));
                        }
                        let mut remaining = old.includes;
                        for include in new.includes {
                            if let Some(index) = remaining.iter().position(|old| old == &include) {
                                remaining.remove(index);
                            } else if include_may_load_test(&include, &path, &test_file) {
                                violations.push(format!("{path}: a new include! may reload the frozen test source"));
                            }
                        }
                    }
                    Err(error) => violations.push(format!("{path}: cannot inspect changed Rust tokens: {error}")),
                }
            }
        }
        if path.split('/').any(|c| c == ".cargo") {
            violations.push(format!("{status} {path}: .cargo/ configuration must not change"));
            continue;
        }
        if guards::base_name(&path) == "Cargo.toml" {
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
// Verification environment
// ---------------------------------------------------------------------------------------

fn controlled_cargo_environment(spec: CommandSpec, cargo_home: &Path, target: &Path) -> CommandSpec {
    let mut spec = spec.scrubbed();
    spec.env
        .retain(|(key, _)| !key.eq_ignore_ascii_case("CARGO_HOME") && !key.eq_ignore_ascii_case("CARGO_TARGET_DIR"));
    spec.env.push(("CARGO_HOME".into(), cargo_home.to_string_lossy().into_owned()));
    spec.env.push(("CARGO_TARGET_DIR".into(), target.to_string_lossy().into_owned()));
    spec
}

fn ensure_empty_cargo_config(cargo_home: &Path) -> Result<(), String> {
    if ["config", "config.toml"].iter().any(|name| std::fs::symlink_metadata(cargo_home.join(name)).is_ok()) {
        return Err("verification Cargo home contains unexpected configuration".into());
    }
    Ok(())
}

/// Cargo searches every ancestor, even HOME/.cargo when CARGO_HOME is overridden.
fn ensure_no_ancestor_cargo_config(worktree: &Path) -> Result<(), String> {
    for ancestor in worktree.parent().into_iter().flat_map(Path::ancestors) {
        for name in ["config", "config.toml"] {
            let path = ancestor.join(".cargo").join(name);
            if std::fs::symlink_metadata(&path).is_ok() {
                return Err(format!(
                    "verification would inherit Cargo configuration from {}; move LYRA_HOME outside that directory",
                    path.display()
                ));
            }
        }
    }
    Ok(())
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
            profile: Some(ID.into()),
            frozen_inputs: Vec::new(),
        }
    }

    fn reader<'a>(map: &'a HashMap<&'a str, &'a str>) -> impl Fn(&str) -> Option<String> + 'a {
        move |p: &str| map.get(p).map(|s| s.to_string())
    }

    /// Cargo writes `Cargo.lock` while running the repro in a library that does not commit
    /// one; it is the toolchain's output, not a test input, so a later dependency fix may
    /// rewrite it. Python has no such file.
    #[test]
    fn cargo_lock_is_toolchain_output_not_a_repro_input() {
        use crate::profile::python_pytest::PythonPytest;
        assert!(RustCargo.is_toolchain_output("Cargo.lock") && RustCargo.is_toolchain_output("crates/x/Cargo.lock"));
        assert!(!RustCargo.is_toolchain_output("tests/fixtures/frame.bin"));
        assert!(!PythonPytest.is_toolchain_output("Cargo.lock"));
    }

    #[test]
    fn detection_requires_a_root_manifest() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect(dir.path()).is_none());
        std::fs::create_dir(dir.path().join("Cargo.toml")).unwrap();
        assert!(detect(dir.path()).is_none(), "a directory named Cargo.toml is not a manifest");
        std::fs::remove_dir(dir.path().join("Cargo.toml")).unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let resolved = detect(dir.path()).unwrap();
        assert_eq!(resolved.id, ID);
        assert_eq!(resolved.evidence, vec!["Cargo.toml".to_string()]);
    }

    #[test]
    fn profile_surface_matches_the_cargo_conventions() {
        let p = RustCargo;
        assert_eq!(p.id(), "rust-cargo");
        assert!(p.required_tools().contains(&"cargo"));
        assert!(p.is_source_file(Path::new("src/lib.rs")));
        assert!(p.is_source_file(Path::new("crates/a/Cargo.toml")));
        assert!(!p.is_source_file(Path::new("README.md")));
        assert!(p.generated_dirs().contains(&"target"));
        let r = rec();
        assert_eq!(p.regression_command_line(&r, Path::new("wt")), format!("cargo test -p foo {NAME} -- --exact"));
        let spec = p.regression_command(&r, Path::new("wt")).unwrap();
        assert_eq!(spec.program, "cargo");
        assert_eq!(spec.args, vec!["test", "-p", "foo", NAME, "--", "--exact"]);
        let fmt = p.format_test(Path::new("wt"), Path::new("wt/tests/issue_123.rs")).unwrap();
        assert_eq!(fmt.program, "rustfmt");
        assert!(fmt.clear_env);
        let repro = p.reproduction_environment(CommandSpec::new("cargo", "wt"), Path::new("run"));
        assert!(repro.clear_env);
        assert!(repro.env.iter().any(|(k, v)| k == "CARGO_TARGET_DIR" && v.ends_with("repro-target")));
        let steps = p.verification_steps(&VerifyCommands::default(), &r, Path::new("wt")).unwrap();
        assert_eq!(steps.iter().map(|s| s.name).collect::<Vec<_>>(), vec!["build", "repro", "tests", "fmt"]);
        assert!(steps[1].regression && !steps[0].regression);
        assert_eq!(steps[1].spec.args, spec.args);
        let empty = VerifyCommands { build: Some(String::new()), ..VerifyCommands::default() };
        assert!(p.verification_steps(&empty, &r, Path::new("wt")).err().unwrap().contains("build"));
        let dir = tempfile::tempdir().unwrap();
        assert!(p.preparation(dir.path(), &|_| true).is_none());
        let prep = p.preparation(dir.path(), &|_| false).unwrap();
        assert_eq!(prep.creates, vec![PathBuf::from("Cargo.lock")]);
        assert_eq!(prep.spec.args, vec!["generate-lockfile"]);
    }

    #[test]
    fn classify_failed_with_panic_message() {
        assert_eq!(classify_cargo_test(FAILED_OUTPUT, NAME), RegressionOutcome::Failed);
        assert!(guards::output_contains_expected(FAILED_OUTPUT, "short frame must be rejected"));
        assert!(assert_test_passed(FAILED_OUTPUT, NAME).unwrap_err().contains("FAILED"));
    }

    #[test]
    fn confirm_failure_requires_one_failing_test_and_cargo_status() {
        let p = RustCargo;
        let failed = CommandOutput { exit_code: Some(101), stdout: FAILED_OUTPUT.into(), ..Default::default() };
        assert!(p.confirm_failure(&failed, &rec()).is_ok());
        let wrong_exit = CommandOutput { exit_code: Some(1), ..failed.clone() };
        assert!(p.confirm_failure(&wrong_exit, &rec()).is_err());
        let two = CommandOutput { stdout: FAILED_OUTPUT.replace("1 failed", "2 failed"), ..failed.clone() };
        assert!(p.confirm_failure(&two, &rec()).is_err());
        let mixed =
            CommandOutput { stdout: FAILED_OUTPUT.replace("0 passed; 1 failed", "1 passed; 1 failed"), ..failed };
        assert!(p.confirm_failure(&mixed, &rec()).is_err());
    }

    #[test]
    fn classify_passed() {
        assert_eq!(classify_cargo_test(PASSED_OUTPUT, NAME), RegressionOutcome::Passed);
        assert_eq!(assert_test_passed(PASSED_OUTPUT, NAME), Ok(()));
        // A module test name is matched exactly as printed.
        let modpath = PASSED_OUTPUT.replace(NAME, "parser::issue_123_tests::issue_123_rejects_short_frame");
        assert_eq!(
            classify_cargo_test(&modpath, "parser::issue_123_tests::issue_123_rejects_short_frame"),
            RegressionOutcome::Passed
        );
        assert_eq!(
            classify_cargo_test(&modpath, NAME),
            RegressionOutcome::Other(format!("test {NAME} did not appear in the output"))
        );
    }

    #[test]
    fn classify_compile_error() {
        assert_eq!(classify_cargo_test(COMPILE_ERROR_OUTPUT, NAME), RegressionOutcome::SetupFailed);
        assert!(assert_test_passed(COMPILE_ERROR_OUTPUT, NAME).unwrap_err().contains("compile"));
        assert_eq!(
            classify_cargo_test("error: could not compile `foo` due to 2 previous errors\n", NAME),
            RegressionOutcome::SetupFailed
        );
    }

    #[test]
    fn classify_filtered_and_ignored() {
        assert_eq!(classify_cargo_test(FILTERED_OUTPUT, NAME), RegressionOutcome::NotDiscovered);
        assert!(assert_test_passed(FILTERED_OUTPUT, NAME).unwrap_err().contains("did not run"));
        assert_eq!(classify_cargo_test(IGNORED_OUTPUT, NAME), RegressionOutcome::Skipped);
        assert!(assert_test_passed(IGNORED_OUTPUT, NAME).unwrap_err().contains("ignored"));
    }

    #[test]
    fn classify_cargo_level_errors_and_empty_output() {
        let out = "error: package ID specification `nope` did not match any packages\n";
        assert_eq!(classify_cargo_test(out, NAME), RegressionOutcome::Other(out.trim().to_string()));
        assert!(matches!(classify_cargo_test("", NAME), RegressionOutcome::Other(_)));
        let crash =
            "running 1 test\nerror: process didn't exit successfully: `target/debug/deps/x` (exit code: 0xc0000005)\n";
        assert!(matches!(
            classify_cargo_test(crash, NAME),
            RegressionOutcome::Other(s) if s.contains("didn't exit successfully")
        ));
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
    fn source_binding_requires_a_complete_empty_test_listing() {
        assert!(assert_test_absent_from_list("0 tests, 0 benchmarks\n", NAME).is_ok());
        assert!(assert_test_absent_from_list(&format!("{NAME}: test\n1 test, 0 benchmarks\n"), NAME).is_err());
        assert!(assert_test_absent_from_list("1 test, 0 benchmarks\n", NAME).is_err());
        assert!(assert_test_absent_from_list("", NAME).is_err());
    }

    #[test]
    fn command_is_built_from_crate_and_name() {
        assert_eq!(repro_test_command(" foo ", NAME), format!("cargo test -p foo {NAME} -- --exact"));
    }

    #[test]
    fn reproduction_selectors_cannot_inject_cargo_arguments() {
        assert_eq!(
            repro_test_args("foo-bar", "parser::issue_123_x").unwrap(),
            vec!["test", "-p", "foo-bar", "parser::issue_123_x", "--", "--exact"]
        );
        for package in ["foo --config=build.rustflags=x", "--workspace", "foo\n--release", ""] {
            assert!(repro_test_args(package, NAME).is_err(), "{package}");
        }
        for name in ["--ignored", "a -- --ignored", "a;echo", "::", "a\nother"] {
            assert!(repro_test_args("foo", name).is_err(), "{name}");
        }
        let mut record = rec();
        record.test_file = "tests/issue_123.py".into();
        assert!(RustCargo.validate_record(&record).unwrap_err().contains("Rust source"));
    }

    #[test]
    fn discovery_lint_cap_preserves_active_flags_and_leaves_strict_command_unchanged() {
        let process_flags = std::env::var_os("RUSTFLAGS");
        let process_encoded = std::env::var_os("CARGO_ENCODED_RUSTFLAGS");
        let mut strict = CommandSpec::new("cargo", ".").args(["test", "--", "--exact"]);
        strict.clear_env = true;
        strict.env.push(("RUSTFLAGS".into(), "--cfg keep_me -Dwarnings".into()));
        let before = strict.env.clone();
        let discovery = source_binding_spec(&strict);
        assert_eq!(
            discovery.env.last().unwrap(),
            &("RUSTFLAGS".into(), "--cfg keep_me -Dwarnings --cap-lints=allow".into())
        );
        assert_eq!(strict.env, before);
        assert_eq!(discovery.args, strict.args);

        strict.env.push(("CARGO_ENCODED_RUSTFLAGS".into(), "--cfg\u{1f}encoded_cfg\u{1f}-Dwarnings".into()));
        let before = strict.env.clone();
        let discovery = source_binding_spec(&strict);
        assert_eq!(
            discovery.env.last().unwrap(),
            &("CARGO_ENCODED_RUSTFLAGS".into(), "--cfg\u{1f}encoded_cfg\u{1f}-Dwarnings\u{1f}--cap-lints=allow".into())
        );
        assert_eq!(strict.env, before);
        strict.env.push(("CARGO_ENCODED_RUSTFLAGS".into(), String::new()));
        assert_eq!(source_binding_spec(&strict).env.last().unwrap().1, "--cap-lints=allow");

        strict.env.clear();
        let discovery = source_binding_spec(&strict);
        assert_eq!(&discovery.args[..2], &["--config", "build.rustflags=[\"--cap-lints=allow\"]"]);
        assert_eq!(&discovery.args[2..], &strict.args);
        assert!(strict.env.is_empty());
        assert_eq!(std::env::var_os("RUSTFLAGS"), process_flags);
        assert_eq!(std::env::var_os("CARGO_ENCODED_RUSTFLAGS"), process_encoded);
        // The trait wrapper appends the listing flag.
        let listing = RustCargo.discovery(&strict);
        assert_eq!(listing.spec.args.last().unwrap(), "--list");
        assert!(listing.note.contains("lint cap"));
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
    fn test_paths() {
        assert!(is_test_path("crates/a/tests/x.rs"));
        assert!(is_test_path("src/parser/issue_7_tests.rs"));
        assert!(is_test_path("src/foo_test.rs"));
        assert!(!is_test_path("src/testsuite.rs"));
        assert!(!is_test_path("tests"));
    }

    const CARGO_OLD: &str = "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n";
    const CARGO_DEV: &str = "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n\n[dev-dependencies]\nproptest = \"1\"\n";
    const CARGO_DEP: &str =
        "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\nbytes = \"1\"\n";
    const LIB_OLD: &str = "pub mod parser;\n\npub fn x() {}\n";
    const LIB_REG: &str = "pub mod parser;\n\n#[cfg(test)]\nmod issue_123_tests;\n\npub fn x() {}\n";
    const LOCK_OLD: &str = "version = 4\n[[package]]\nname = \"foo\"\nversion = \"0.1.0\"\ndependencies = [\"dep\"]\n[[package]]\nname = \"dep\"\nversion = \"2.0.0\"\nsource = \"registry+https://example.invalid\"\nchecksum = \"original\"\n";

    #[test]
    fn repro_changes_allowed_set() {
        let changes = vec![
            ("??".to_string(), "tests/issue_123.rs".to_string()),
            ("??".to_string(), "tests/fixtures/short.bin".to_string()),
            ("A".to_string(), "crates/bar/fixtures/data.json".to_string()),
            ("M".to_string(), "Cargo.toml".to_string()),
            ("M".to_string(), "Cargo.lock".to_string()),
        ];
        let old: HashMap<&str, &str> = HashMap::from([("Cargo.toml", CARGO_OLD), ("Cargo.lock", LOCK_OLD)]);
        let new: HashMap<&str, &str> = HashMap::from([("Cargo.toml", CARGO_DEV), ("Cargo.lock", LOCK_OLD)]);
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
    fn repro_lockfile_allows_test_additions_but_not_downgrades_or_rewrites() {
        let addition = "\n[[package]]\nname = \"test-helper\"\nversion = \"1.0.0\"\nsource = \"registry+https://example.invalid\"\nchecksum = \"new\"\n";
        let new = format!("{}{addition}", LOCK_OLD.replace("[\"dep\"]", "[\"dep\", \"test-helper\"]"));
        assert!(cargo_lock_only_additions(Some(LOCK_OLD), &new));
        assert!(cargo_lock_only_additions(None, LOCK_OLD));
        assert!(!cargo_lock_only_additions(Some(LOCK_OLD), &LOCK_OLD.replace("2.0.0", "1.0.0")));
        assert!(!cargo_lock_only_additions(Some(LOCK_OLD), &LOCK_OLD.replace("original", "replaced")));
        assert!(!cargo_lock_only_additions(Some(LOCK_OLD), &LOCK_OLD.replace("[\"dep\"]", "[]")));
        assert!(!cargo_lock_only_additions(Some(LOCK_OLD), "version = 4\npackage = []"));
        assert!(!cargo_lock_only_additions(Some(LOCK_OLD), "not toml"));
    }

    #[test]
    fn fix_cannot_redirect_or_disable_frozen_test_module() {
        let mut record = rec();
        record.test_file = "src/issue_123_tests.rs".into();
        record.registration_file = Some("src/lib.rs".into());
        let frozen =
            HashMap::from([("src/issue_123_tests.rs", "#[test] fn issue_123_x() {}"), ("src/lib.rs", LIB_REG)]);
        let changes = vec![("M".into(), "src/lib.rs".into())];
        for injected in [
            "#[path = \"evil.rs\"]\n#[cfg(test)]",
            "#[cfg(any())]\n#[cfg(test)]",
            "#[cfg(\n    any()\n)]\n#[cfg(test)]",
            "#[cfg_attr(\n    test, path = \"evil.rs\"\n)] // redirect\n#[cfg(test)]",
            "#[cfg_attr(\n    test, path = \"evil.rs\"\n)] /* redirect */\n#[cfg(test)]",
            "#[path = \"evil.rs\"]\n// hide attribute\n#[cfg(test)]",
        ] {
            let changed = LIB_REG.replace("#[cfg(test)]", injected);
            let current = HashMap::from([
                ("src/issue_123_tests.rs", "#[test] fn issue_123_x() {}"),
                ("src/lib.rs", changed.as_str()),
            ]);
            let error = check_fix_changes_with(&changes, &record, &reader(&frozen), &reader(&current)).unwrap_err();
            assert!(error.contains("attribute block"), "{error}");
        }
        assert!(!is_registration_only_change(
            "pub fn production() {}\n",
            "#[cfg(test)]\npub fn production() {}\n#[cfg(test)] mod issue_123_tests;\n"
        ));
    }

    #[test]
    fn fix_cannot_hide_registration_in_a_macro_and_reinclude_with_shadow_functions() {
        let mut record = rec();
        record.test_file = "src/issue_123_tests.rs".into();
        record.registration_file = Some("src/lib.rs".into());
        let original = "fn buggy_fn() -> bool { false }\n#[cfg(test)]\nmod issue_123_tests;\n";
        let attack = "fn buggy_fn() -> bool { false }\nmacro_rules! hide { () => {\n#[cfg(test)]\nmod issue_123_tests;\n} }\n#[cfg(test)] mod issue_123_tests { fn buggy_fn() -> bool { true } include!(\"issue_123_tests.rs\"); }\n";
        // The earlier line-based guard considered the unused macro's declaration frozen.
        assert_eq!(registration_block(original, Some(123)), registration_block(attack, Some(123)));
        let test = "use super::*; #[test] fn issue_123_rejects_short_frame() { assert!(buggy_fn()); }";
        let frozen = HashMap::from([("src/lib.rs", original), ("src/issue_123_tests.rs", test)]);
        let current = HashMap::from([("src/lib.rs", attack), ("src/issue_123_tests.rs", test)]);
        let error =
            check_fix_changes_with(&[("M".into(), "src/lib.rs".into())], &record, &reader(&frozen), &reader(&current))
                .unwrap_err();
        assert!(error.contains("registration line"), "{error}");
    }

    #[test]
    fn fix_rejects_macro_duplicate_modules_and_new_includes_of_frozen_source() {
        let mut record = rec();
        record.test_file = "src/issue_123_tests.rs".into();
        record.registration_file = Some("src/lib.rs".into());
        let frozen = HashMap::from([("src/lib.rs", LIB_REG), ("src/issue_123_tests.rs", "#[test] fn regression() {}")]);
        for (changed, expected) in [
            (format!("{LIB_REG}\nmacro_rules! duplicate {{ () => {{ mod issue_123_tests {{}} }} }}"), "another declaration"),
            (format!("{LIB_REG}\nmod shadow {{ include!(\"issue_123_tests.rs\"); }}"), "new include!"),
            (format!("{LIB_REG}\nmod shadow {{ include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/src/issue_123_tests.rs\")); }}"), "new include!"),
        ] {
            let current = HashMap::from([("src/lib.rs", changed.as_str()), ("src/issue_123_tests.rs", "#[test] fn regression() {}")]);
            let error = check_fix_changes_with(&[("M".into(), "src/lib.rs".into())], &record, &reader(&frozen), &reader(&current)).unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
        let harmless = format!("{LIB_REG}\nfn fixed() {{ let _ = \"mod issue_123_tests; include!(other)\"; }}\ninclude!(\"generated.rs\");");
        let current = HashMap::from([
            ("src/lib.rs", harmless.as_str()),
            ("src/issue_123_tests.rs", "#[test] fn regression() {}"),
        ]);
        check_fix_changes_with(&[("M".into(), "src/lib.rs".into())], &record, &reader(&frozen), &reader(&current))
            .unwrap();
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

    #[test]
    fn controlled_cargo_paths_override_inherited_configuration() {
        let root = Path::new("env-root");
        let spec = RustCargo.command_environment(
            CommandSpec::new("cargo", ".")
                .env("CARGO_HOME", "untrusted-home")
                .env("CARGO_TARGET_DIR", "untrusted-target"),
            root,
        );
        assert!(spec.clear_env);
        assert_eq!(
            spec.env.iter().filter(|(k, _)| k.eq_ignore_ascii_case("CARGO_HOME")).collect::<Vec<_>>(),
            vec![&("CARGO_HOME".into(), root.join("cargo-home").to_string_lossy().into_owned())]
        );
        assert!(spec.env.iter().any(|(k, v)| k == "CARGO_TARGET_DIR" && v.ends_with("target")));
        assert!(!spec.env.iter().any(|(_, value)| value.starts_with("untrusted-")));
    }

    #[test]
    fn ancestor_and_injected_cargo_configuration_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("run").join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::create_dir_all(dir.path().join(".cargo")).unwrap();
        std::fs::write(dir.path().join(".cargo/config.toml"), "[build]\nrustc-wrapper = 'fake'").unwrap();
        assert!(RustCargo.check_environment_root(&checkout).unwrap_err().contains("inherit Cargo"));
        let env_root = dir.path().join("env");
        RustCargo.create_environment(&env_root).unwrap();
        assert!(env_root.join("cargo-home").is_dir());
        assert!(RustCargo.check_environment(&env_root).is_ok());
        std::fs::write(env_root.join("cargo-home/config"), "[alias]\ntest = 'fake'").unwrap();
        assert!(RustCargo.check_environment(&env_root).is_err());
    }

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
        assert_eq!(RustCargo.explain_failure("", None), "process was killed");
    }
}
