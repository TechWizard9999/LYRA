//! Execution profiles (PLAN §13): everything the harness must know about the *target*
//! repository's toolchain and test runner, behind one runner-neutral contract. The
//! engine, the seven stages and the guards call this contract; they never name Cargo.
//!
//! Two concerns live behind the same trait object because one built-in profile serves
//! both today, but they are kept distinct in the method groups below: the
//! **project/toolchain profile** (source and manifest patterns, repository description,
//! environment preparation, verification commands) and the **test-runner integration**
//! (exact test identity, selection, discovery binding, evidence classification, the
//! repro and fix guards). A language alone never picks a runner.
//!
//! Resolution happens once, in intake, from files that exist in the worktree. The
//! result is saved as [`ResolvedProfile`] in `run.json` and `artifacts/profile.json`
//! and is frozen for the run. Saved runs from before this contract carry no profile;
//! [`for_state`] maps them explicitly to the only profile that ever existed.

pub mod rust_cargo;

use crate::config::VerifyConfig;
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
    /// Executables `lyra doctor` checks for this profile.
    fn required_tools(&self) -> &'static [&'static str];

    // --- project / toolchain -------------------------------------------------------

    /// Source files supplied to the agent in line-numbered snapshots.
    fn is_source_file(&self, path: &Path) -> bool;
    /// File extensions (without the dot) that issue text may reference as source paths.
    fn source_extensions(&self) -> &'static [&'static str];
    /// Manifest file names supplied alongside a relevant source file's ancestors.
    fn manifest_names(&self) -> &'static [&'static str];
    /// Directory names of build outputs, skipped in source overviews.
    fn generated_dirs(&self) -> &'static [&'static str];
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
    /// The verification commands, in order, built in `worktree`.
    fn verification_steps(
        &self,
        verify: &VerifyConfig,
        rec: &ReproRecord,
        worktree: &Path,
    ) -> Result<Vec<VerifyStep>, String>;
    /// The line that best explains a failed command, or the exit code.
    fn explain_failure(&self, output: &str, exit_code: Option<i32>) -> String;
}

/// All built-in profiles, in detection order.
pub fn builtin() -> Vec<Box<dyn ExecutionProfile>> {
    vec![Box::new(rust_cargo::RustCargo)]
}

/// The built-in profile with this id.
pub fn by_id(id: &str) -> Option<Box<dyn ExecutionProfile>> {
    builtin().into_iter().find(|p| p.id() == id)
}

/// Detects the profile of an existing worktree from the files present. Explicit
/// repository/operator configuration takes precedence in later phases; today the
/// built-in detectors are the only source, and nothing is executed.
pub fn resolve(worktree: &Path) -> Result<(ResolvedProfile, Box<dyn ExecutionProfile>), String> {
    if let Some(resolved) = rust_cargo::detect(worktree) {
        return Ok((resolved, Box::new(rust_cargo::RustCargo)));
    }
    Err(format!(
        "no built-in execution profile matches this repository (looked for {}); supported profiles: {}. \
         Other languages onboard through a profile, see docs/PLAN.md §13",
        rust_cargo::DETECTION,
        builtin().iter().map(|p| p.id()).collect::<Vec<_>>().join(", ")
    ))
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
            },
            "claude".into(),
        )
    }

    #[test]
    fn cargo_manifest_at_the_root_resolves_to_rust_cargo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let (resolved, profile) = resolve(dir.path()).unwrap();
        assert_eq!(resolved.id, "rust-cargo");
        assert_eq!(resolved.version, CONTRACT_VERSION);
        assert_eq!(resolved.project_dir, "");
        assert_eq!(resolved.evidence, vec!["Cargo.toml".to_string()]);
        assert_eq!(profile.id(), "rust-cargo");
    }

    #[test]
    fn unsupported_repository_fails_with_an_actionable_message() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        let error = resolve(dir.path()).err().unwrap();
        assert!(error.contains("Cargo.toml"), "{error}");
        assert!(error.contains("rust-cargo"), "{error}");
        assert!(error.contains("PLAN.md"), "{error}");
        // A manifest below the root is not a root project.
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/Cargo.toml"), "").unwrap();
        assert!(resolve(dir.path()).is_err());
    }

    #[test]
    fn legacy_runs_map_explicitly_to_rust_cargo_and_unknown_ids_fail() {
        let mut st = state();
        assert!(st.profile.is_none());
        assert_eq!(for_state(&st).unwrap().id(), "rust-cargo");
        st.profile = Some(ResolvedProfile {
            id: "python-pytest".into(),
            version: CONTRACT_VERSION,
            project_dir: String::new(),
            evidence: vec![],
        });
        let error = for_state(&st).err().unwrap();
        assert!(error.contains("python-pytest"), "{error}");
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
