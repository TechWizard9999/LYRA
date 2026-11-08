//! Execution profiles (PLAN §13): everything the harness must know about the *target*
//! repository's toolchain and test runner, behind one runner-neutral contract. The
//! engine, the seven stages and the guards call this contract; they never name Cargo.
//!
//! Two concerns live behind the same trait object because each built-in profile pairs one
//! toolchain with one runner today, but they are kept distinct in the method groups below: the
//! **project/toolchain profile** (source and manifest patterns, repository description,
//! environment preparation, verification commands) and the **test-runner integration**
//! (exact test identity, selection, discovery binding, evidence classification, the
//! repro and fix guards). A language alone never picks a runner.
//!
//! Resolution happens once, in intake, from files that exist in the worktree. The
//! result is saved as [`ResolvedProfile`] in `run.json` and `artifacts/profile.json`
//! and is frozen for the run. Saved runs from before this contract carry no profile;
//! [`for_state`] maps them explicitly to `rust-cargo`, the only profile that existed then.

pub mod python_pytest;
pub mod rust_cargo;

use crate::config::VerifyCommands;
use crate::guards::ReproRecord;
use crate::proc::{CommandOutput, CommandSpec};
use crate::state::RunState;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Bumped when the saved [`ResolvedProfile`] shape or the evidence contract changes.
pub const CONTRACT_VERSION: u32 = 1;

/// The resolved, frozen profile of one run. Names and file paths only: never file
/// contents, environment values or secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedProfile {
    /// Built-in profile id, e.g. `rust-cargo`.
    pub id: String,
    pub version: u32,
    /// Worktree-relative project directory; `""` is the repository root.
    #[serde(default)]
    pub project_dir: String,
    /// Why this profile was chosen (worktree-relative files that were found).
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// Runner-neutral result of running the one selected regression test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegressionOutcome {
    /// The test ran and failed on an assertion or panic: the only acceptable S2 result.
    Failed,
    Passed,
    /// Skipped, ignored or expected-failure: neither proof nor a pass.
    Skipped,
    /// Nothing ran: wrong selector, or the test is not discovered by the runner.
    NotDiscovered,
    /// Compilation, collection or setup failed before the test could run.
    SetupFailed,
    /// Anything else (runner error, crashed test binary, missing summary), with the reason.
    Other(String),
}

/// A discovery-only command that must *not* find the selected test once its source
/// file is temporarily emptied (PLAN §3.1 source binding), plus a note for the log.
pub struct Discovery {
    pub spec: CommandSpec,
    pub note: &'static str,
}

/// An optional preparation step verify runs before freezing the checkout baseline
/// (for example generating a missing lockfile). `creates` lists the only files it may add.
pub struct Preparation {
    pub name: &'static str,
    pub spec: CommandSpec,
    /// Artifact path of the log, relative to `artifacts/`.
    pub log: &'static str,
    pub creates: Vec<PathBuf>,
}

/// One verification command in the fresh checkout, in order.
pub struct VerifyStep {
    pub name: &'static str,
    pub spec: CommandSpec,
    /// Artifact path of the log, relative to `artifacts/`.
    pub log: &'static str,
    /// The selected regression test itself: its output is also checked with `assert_passed`.
    pub regression: bool,
}

/// `read(path) -> Some(content)` for a repository-relative path, `None` when absent.
pub type FileReader<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The contract. Every method is pure string/path logic or builds a [`CommandSpec`];
/// the harness runs the commands and owns timeouts, cancellation and evidence files.
#[async_trait]
pub trait ExecutionProfile: Send + Sync {
    fn id(&self) -> &'static str;
    /// This profile's evidence in an existing worktree, read from files only; `None` when absent.
    fn detect(&self, worktree: &Path) -> Option<ResolvedProfile>;
    /// What [`Self::detect`] looks for, quoted in diagnostics.
    fn detection(&self) -> &'static str;
    /// Executables Intake requires for a run with this profile; `lyra doctor` reports them.
    fn required_tools(&self) -> &'static [&'static str];
    /// The target language as prompts name it ("Rust", "Python").
    fn language(&self) -> &'static str;
    /// The profile's prompt passages: a minijinja file of macros from `src/prompts/profiles/`
    /// that every stage prompt imports as `profile` (see PLAN §13.4).
    fn prompt_macros(&self) -> &'static str;

    // --- project / toolchain -------------------------------------------------------

    /// Source files supplied to the agent in line-numbered snapshots.
    fn is_source_file(&self, path: &Path) -> bool;
    /// File extensions (without the dot) that issue text may reference as source paths.
    fn source_extensions(&self) -> &'static [&'static str];
    /// Manifest file names supplied alongside a relevant source file's ancestors.
    fn manifest_names(&self) -> &'static [&'static str];
    /// Directory names of build outputs, skipped in source overviews.
    fn generated_dirs(&self) -> &'static [&'static str];
    /// Whether the directory at `path` (`relative` to the repository root) holds generated
    /// output rather than source. The default matches [`Self::generated_dirs`] by name;
    /// profiles whose tooling uses common words as output names decide by markers instead.
    fn is_generated_dir(&self, relative: &Path, _path: &Path) -> bool {
        relative.file_name().and_then(|n| n.to_str()).is_some_and(|name| self.generated_dirs().contains(&name))
    }
    /// A path the toolchain or convention treats as test code.
    fn is_test_path(&self, path: &str) -> bool;
    /// Workspace/project layout for `10-context/repo.json` (best effort: errors are values).
    async fn describe_repository(
        &self,
        worktree: &Path,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> serde_json::Value;

    // --- regression identity and evidence -----------------------------------------

    /// Profile-specific checks on the agent's record (selector syntax, test file type).
    fn validate_record(&self, rec: &ReproRecord) -> Result<(), String>;
    /// The one command that runs exactly the selected test, as a display line.
    fn regression_command_line(&self, rec: &ReproRecord) -> String;
    /// The same command with validated selectors as separate arguments.
    fn regression_command(&self, rec: &ReproRecord, cwd: &Path) -> Result<CommandSpec, String>;
    /// Turns a regression command into its discovery-only form.
    fn discovery(&self, spec: &CommandSpec) -> Discovery;
    /// Formats the new test file before it is executed and frozen, if the toolchain has a formatter.
    fn format_test(&self, worktree: &Path, test_file: &Path) -> Option<CommandSpec>;
    fn classify(&self, output: &str, rec: &ReproRecord) -> RegressionOutcome;
    fn describe_outcome(&self, outcome: &RegressionOutcome, rec: &ReproRecord) -> String;
    /// S2: a `Failed` classification must also be exactly one failing case and the
    /// runner's test-failure exit status, so a crashed or partial run is not a reproduction.
    fn confirm_failure(&self, run: &CommandOutput, rec: &ReproRecord) -> Result<(), String>;
    /// S5: the selected test passed and exactly one test ran.
    fn assert_passed(&self, output: &str, rec: &ReproRecord) -> Result<(), String>;
    /// Source binding: the selected test is absent from a discovery run with its source emptied.
    fn assert_absent_from_discovery(&self, output: &str, rec: &ReproRecord) -> Result<(), String>;
    /// S2: files the failing regression run actually loaded that are not production code
    /// (repository-relative), which Repro freezes with the test; `worktree` is the checkout the
    /// run used, for the profile to read what the repository declares. An error means the
    /// profile cannot establish them and the reproduction is rejected. The default records
    /// nothing (`rust-cargo` locks its test inputs through registration and the frozen-input
    /// rule).
    fn loaded_test_inputs(&self, _output: &str, _rec: &ReproRecord, _worktree: &Path) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    // --- guards -----------------------------------------------------------------------

    /// Allowed S2 changes (PLAN §3.1). `changes` is `(status, path)` from git.
    fn check_repro_changes(
        &self,
        changes: &[(String, String)],
        rec: &ReproRecord,
        read_old: FileReader<'_>,
        read_new: FileReader<'_>,
    ) -> Result<(), String>;
    /// The test lock (PLAN §3.3): frozen test, registration, runner configuration, dependencies.
    fn check_fix_changes(
        &self,
        changes: &[(String, String)],
        rec: &ReproRecord,
        read_checkpoint: FileReader<'_>,
        read_now: FileReader<'_>,
    ) -> Result<(), String>;

    // --- environments --------------------------------------------------------------

    /// S2: where build outputs of the agent's worktree go (outside the worktree).
    fn reproduction_environment(&self, spec: CommandSpec, run_root: &Path) -> CommandSpec;
    /// Rejects a checkout location whose ancestors would inject toolchain configuration.
    fn check_environment_root(&self, checkout: &Path) -> Result<(), String>;
    /// Creates the owned verification environment (caches, outputs) under `env_root`.
    fn create_environment(&self, env_root: &Path) -> std::io::Result<()>;
    /// Rejects unexpected configuration inside the owned environment.
    fn check_environment(&self, env_root: &Path) -> Result<(), String>;
    /// Scrubs the command and points the toolchain at the owned environment.
    fn command_environment(&self, spec: CommandSpec, env_root: &Path) -> CommandSpec;
    /// An optional baseline preparation when the checkout lacks a generated input;
    /// `present(path)` tells whether a worktree-relative file exists.
    fn preparation(&self, worktree: &Path, present: &dyn Fn(&Path) -> bool) -> Option<Preparation>;
    /// The verification commands, in order, built in `worktree`. `verify` holds the configured
    /// commands for this profile (`None` = the profile's default; see `Config::verify_commands`).
    fn verification_steps(
        &self,
        verify: &VerifyCommands,
        rec: &ReproRecord,
        worktree: &Path,
    ) -> Result<Vec<VerifyStep>, String>;
    /// The line that best explains a failed command, or the exit code.
    fn explain_failure(&self, output: &str, exit_code: Option<i32>) -> String;
}

/// All built-in profiles, in detection order.
pub fn builtin() -> Vec<Box<dyn ExecutionProfile>> {
    vec![Box::new(rust_cargo::RustCargo), Box::new(python_pytest::PythonPytest)]
}

/// The built-in profile with this id.
pub fn by_id(id: &str) -> Option<Box<dyn ExecutionProfile>> {
    builtin().into_iter().find(|p| p.id() == id)
}

/// The outcome of profile resolution: the chosen profile plus every other built-in profile
/// whose evidence was also found, so an ambiguous repository is visible rather than silent.
pub struct Resolution {
    pub resolved: ResolvedProfile,
    pub profile: Box<dyn ExecutionProfile>,
    /// `"detection"` (first matching built-in profile) or `"request"` (chosen by the operator).
    pub selected_by: &'static str,
    /// Other built-in profiles whose evidence was found, in detection order.
    pub alternatives: Vec<ResolvedProfile>,
}

impl Resolution {
    /// `artifacts/profile.json`: the saved [`ResolvedProfile`] plus how it was selected and
    /// which alternatives were detected. `run.json` keeps only the [`ResolvedProfile`].
    pub fn artifact(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(&self.resolved).unwrap_or_default();
        value["selected_by"] = serde_json::Value::from(self.selected_by);
        value["alternatives"] = serde_json::to_value(&self.alternatives).unwrap_or_default();
        value
    }
}

/// Resolves the profile of an existing worktree from the files present. Nothing is executed.
///
/// - `requested = Some(id)`: the operator's explicit choice (`lyra run --profile`, the
///   dashboard, `POST /api/runs`). The id must be a built-in profile and its evidence must be
///   present; a choice is never a way to run a runner the repository does not use.
/// - `requested = None`: the first built-in profile in [`builtin`] order whose evidence is
///   present. A root `Cargo.toml` therefore wins over pytest evidence, which keeps existing Rust
///   runs unchanged; the other matches are reported as `alternatives`.
pub fn resolve(worktree: &Path, requested: Option<&str>) -> Result<Resolution, String> {
    let mut matches: Vec<(ResolvedProfile, Box<dyn ExecutionProfile>)> =
        builtin().into_iter().filter_map(|p| p.detect(worktree).map(|r| (r, p))).collect();
    let supported = builtin().iter().map(|p| p.id()).collect::<Vec<_>>().join(", ");
    let index = match requested.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => {
            let Some(wanted) = by_id(id) else {
                return Err(format!("unknown execution profile '{id}'; supported profiles: {supported}"));
            };
            matches.iter().position(|(r, _)| r.id == id).ok_or_else(|| {
                format!(
                    "execution profile '{id}' was requested but this repository has none of its evidence (looked for {})",
                    wanted.detection()
                )
            })?
        }
        None if matches.is_empty() => {
            let looked_for = builtin().iter().map(|p| p.detection()).collect::<Vec<_>>().join("; or ");
            return Err(format!(
                "no built-in execution profile matches this repository (looked for {looked_for}); supported profiles: {supported}. \
                 Other languages onboard through a profile, see docs/PLAN.md §13"
            ));
        }
        None => 0,
    };
    let (resolved, profile) = matches.remove(index);
    Ok(Resolution {
        resolved,
        profile,
        selected_by: if requested.is_some_and(|id| !id.trim().is_empty()) { "request" } else { "detection" },
        alternatives: matches.into_iter().map(|(r, _)| r).collect(),
    })
}

/// Executables from `profile.required_tools()` that cannot be found on `PATH`.
pub fn missing_tools(profile: &dyn ExecutionProfile) -> Vec<&'static str> {
    profile.required_tools().iter().copied().filter(|tool| crate::proc::resolve_program(tool).is_err()).collect()
}

/// The profile a saved run was resolved with. Runs written before this contract have
/// no `profile`; every one of them was a Cargo run, so they map to `rust-cargo`.
pub fn for_state(state: &RunState) -> Result<Box<dyn ExecutionProfile>, String> {
    match &state.profile {
        None => Ok(Box::new(rust_cargo::RustCargo)),
        Some(resolved) => {
            if resolved.version != CONTRACT_VERSION {
                return Err(format!(
                    "run was resolved with execution profile contract v{} ('{}'); this LYRA implements v{CONTRACT_VERSION} \
                     and does not apply its rules to a run saved under another contract",
                    resolved.version, resolved.id
                ));
            }
            by_id(&resolved.id).ok_or_else(|| {
                format!(
                    "run was resolved with execution profile '{}' (contract v{}), which this LYRA does not provide",
                    resolved.id, resolved.version
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RunRequest;

    fn state() -> RunState {
        RunState::new(
            "r".into(),
            RunRequest {
                repo: PathBuf::from("/tmp/repo"),
                issue: "#1".into(),
                issue_text: None,
                provider: None,
                policy_acknowledged: false,
                profile: None,
            },
            "claude".into(),
        )
    }

    #[test]
    fn cargo_manifest_at_the_root_resolves_to_rust_cargo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let resolution = resolve(dir.path(), None).unwrap();
        let resolved = &resolution.resolved;
        assert_eq!(resolved.id, "rust-cargo");
        assert_eq!(resolved.version, CONTRACT_VERSION);
        assert_eq!(resolved.project_dir, "");
        assert_eq!(resolved.evidence, vec!["Cargo.toml".to_string()]);
        assert_eq!(resolution.profile.id(), "rust-cargo");
        assert_eq!(resolution.selected_by, "detection");
        assert!(resolution.alternatives.is_empty());
    }

    #[test]
    fn mixed_repositories_default_to_cargo_report_alternatives_and_accept_an_explicit_choice() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.path().join("pytest.ini"), "[pytest]\n").unwrap();

        // Unchanged default for legacy and unambiguous behaviour, with the other match visible.
        let default = resolve(dir.path(), None).unwrap();
        assert_eq!((default.resolved.id.as_str(), default.selected_by), ("rust-cargo", "detection"));
        assert_eq!(default.alternatives.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["python-pytest"]);
        let artifact = default.artifact();
        assert_eq!(artifact["alternatives"][0]["evidence"], serde_json::json!(["pytest.ini"]));
        assert_eq!(artifact["selected_by"], "detection");
        // The saved run.json shape is unchanged: only ResolvedProfile is stored there.
        assert!(serde_json::to_value(&default.resolved).unwrap().get("alternatives").is_none());

        // Either supported runner can be targeted explicitly.
        let python = resolve(dir.path(), Some("python-pytest")).unwrap();
        assert_eq!((python.resolved.id.as_str(), python.selected_by), ("python-pytest", "request"));
        assert_eq!(python.alternatives.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["rust-cargo"]);
        assert_eq!(resolve(dir.path(), Some("rust-cargo")).unwrap().resolved.id, "rust-cargo");
        assert_eq!(resolve(dir.path(), Some(" ")).unwrap().selected_by, "detection", "blank means auto-detect");

        // A choice needs that profile's evidence and a known id.
        std::fs::remove_file(dir.path().join("pytest.ini")).unwrap();
        let error = resolve(dir.path(), Some("python-pytest")).err().unwrap();
        assert!(error.contains("none of its evidence") && error.contains("pytest.ini"), "{error}");
        assert!(resolve(dir.path(), Some("go-test")).err().unwrap().contains("unknown execution profile"));
    }

    #[test]
    fn unsupported_repository_fails_with_an_actionable_message() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        let error = resolve(dir.path(), None).err().unwrap();
        assert!(error.contains("Cargo.toml"), "{error}");
        assert!(error.contains("rust-cargo"), "{error}");
        assert!(error.contains("PLAN.md"), "{error}");
        // A manifest below the root is not a root project.
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/Cargo.toml"), "").unwrap();
        assert!(resolve(dir.path(), None).is_err());
    }

    #[test]
    fn legacy_runs_map_explicitly_to_rust_cargo_and_unknown_ids_fail() {
        let mut st = state();
        assert!(st.profile.is_none());
        assert_eq!(for_state(&st).unwrap().id(), "rust-cargo");
        st.profile = Some(ResolvedProfile {
            id: "go-test".into(),
            version: CONTRACT_VERSION,
            project_dir: String::new(),
            evidence: vec![],
        });
        let error = for_state(&st).err().unwrap();
        assert!(error.contains("go-test"), "{error}");
        st.profile = Some(ResolvedProfile {
            id: "python-pytest".into(),
            version: CONTRACT_VERSION,
            project_dir: String::new(),
            evidence: vec!["pytest.ini".into()],
        });
        assert_eq!(for_state(&st).unwrap().id(), "python-pytest");
        st.profile = Some(ResolvedProfile {
            id: "rust-cargo".into(),
            version: CONTRACT_VERSION,
            project_dir: String::new(),
            evidence: vec!["Cargo.toml".into()],
        });
        assert_eq!(for_state(&st).unwrap().id(), "rust-cargo");
    }

    #[test]
    fn unsupported_saved_contract_versions_are_rejected_even_for_known_ids() {
        let mut st = state();
        for version in [0, CONTRACT_VERSION + 1, 999] {
            st.profile = Some(ResolvedProfile {
                id: "rust-cargo".into(),
                version,
                project_dir: String::new(),
                evidence: vec!["Cargo.toml".into()],
            });
            let error = for_state(&st).err().unwrap();
            assert!(error.contains(&format!("contract v{version}")), "{error}");
            assert!(error.contains(&format!("implements v{CONTRACT_VERSION}")), "{error}");
        }
        // The version check runs before the id lookup, so a foreign id under a foreign version
        // reports the contract mismatch first.
        st.profile = Some(ResolvedProfile {
            id: "python-pytest".into(),
            version: CONTRACT_VERSION + 1,
            project_dir: String::new(),
            evidence: vec![],
        });
        assert!(for_state(&st).err().unwrap().contains("contract v2"));
    }

    #[test]
    fn resolved_profile_roundtrips_and_tolerates_missing_optional_fields() {
        let full = ResolvedProfile {
            id: "rust-cargo".into(),
            version: 1,
            project_dir: "crates/a".into(),
            evidence: vec!["crates/a/Cargo.toml".into()],
        };
        let json = serde_json::to_string(&full).unwrap();
        assert_eq!(serde_json::from_str::<ResolvedProfile>(&json).unwrap(), full);
        let minimal: ResolvedProfile = serde_json::from_str(r#"{"id":"rust-cargo","version":1}"#).unwrap();
        assert_eq!(minimal.project_dir, "");
        assert!(minimal.evidence.is_empty());
    }

    #[test]
    fn builtin_ids_are_unique_and_lookup_works() {
        let ids: Vec<&str> = builtin().iter().map(|p| p.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len());
        assert!(by_id("rust-cargo").is_some());
        assert!(by_id("nope").is_none());
    }
}
