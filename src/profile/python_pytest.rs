//! The `python-pytest` execution profile (PLAN §13.6): Python projects whose tests run under
//! pytest. A language alone never picks a runner, so detection needs pytest evidence: a pytest
//! configuration file or section, a root `conftest.py`, or pytest declared as a dependency.
//!
//! The selected regression test is identified by its pytest node id
//! (`tests/test_issue_7.py::test_issue_7_rejects_short_frame`). Every run neutralises the
//! repository's `addopts`, disables the cache provider and pins the root directory, so the
//! output the harness parses has one known shape and nothing is written into the checkout.
//!
//! Dependencies are not installed by LYRA: the interpreter on `PATH` when LYRA starts
//! (`python3`, or `python` on Windows) must already have pytest and the project's
//! dependencies, for example an activated virtual environment.

use super::{Discovery, ExecutionProfile, FileReader, Preparation, RegressionOutcome, ResolvedProfile, VerifyStep};
use crate::config::VerifyCommands;
use crate::guards::{self, ReproRecord};
use crate::proc::{self, CommandOutput, CommandSpec};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const ID: &str = "python-pytest";
/// What `detect` looks for; quoted in the unsupported-repository diagnostic.
pub const DETECTION: &str = "pytest evidence at the repository root (pytest.ini, pytest.toml, [tool.pytest] in \
                             pyproject.toml, [tool:pytest] in setup.cfg, [pytest] in tox.ini, conftest.py, or pytest \
                             in pyproject.toml / requirements*.txt)";

/// The interpreter LYRA runs; whatever environment is active on `PATH` provides pytest.
pub const PYTHON: &str = if cfg!(windows) { "python" } else { "python3" };

/// Flags on every pytest invocation the harness owns: no `.pytest_cache` in the checkout,
/// the repository's `addopts` neutralised (they may add `-x`, `-n auto`, `--lf` or quiet
/// modes that change what runs or what is printed), node ids relative to the repository
/// root, no `conftest.py` loaded from above it, plain output and a full short summary.
const PYTEST_FLAGS: [&str; 8] =
    ["-p", "no:cacheprovider", "-o", "addopts=", "--rootdir=.", "--confcutdir=.", "--color=no", "-rA"];

/// Discovery runs pytest in-process so "no tests collected" (pytest exit 5, the expected
/// result once the test source is emptied) counts as a completed discovery run; every other
/// exit status is passed through unchanged.
const DISCOVERY_WRAPPER: &str =
    "import sys, pytest; code = int(pytest.main(sys.argv[1:])); sys.exit(0 if code == 5 else code)";

/// The regression runs pytest in-process through this fixed recorder, which afterwards lists
/// every module file it loaded from inside the working directory (`LYRA-LOADED`), every file
/// inside it whose existing content the run may have consumed (`LYRA-READ`), and every child
/// process it started (`LYRA-SPAWN`), all observed through an audit hook installed before
/// pytest starts:
///
/// - Relative paths are resolved against the working directory at the moment of the open, so
///   a test that changes directory (`os.chdir("fixtures"); open("case.json")`) is recorded as
///   reading `fixtures/case.json`; if that directory no longer exists the path is reported as
///   unresolved and Repro rejects the run. (`os.open(..., dir_fd=...)` is not visible in the
///   audit event; any such file is still covered by the fix-stage allowlist.)
/// - `open` (raised by `open`, `io.open`, `os.open` and `pathlib`) counts as a read when the
///   file already existed and the open can read it without truncating it first: `r`, `r+`,
///   `a+`, `rb`… and `os.open` with `O_RDONLY` or `O_RDWR` without `O_TRUNC`. Only `w`/`w+`
///   (and `O_TRUNC`), which discard the content, and write-only opens are not reads.
/// - `sqlite3.connect` of an existing database file counts as a read.
/// - pytest itself reads the root `pyproject.toml`, `setup.cfg` and `tox.ini` while it
///   configures, before the session starts (marked by a `pytest_sessionstart` hook); those
///   reads are not regression inputs. The same file read during the session (the test parses
///   its own manifest) is one, and is frozen whole, dependency fields included.
/// - `subprocess.Popen`, `os.system`, `os.exec`, `os.posix_spawn`, `os.spawn`, `os.fork`,
///   `os.forkpty` and `os.startfile` start a process whose reads this interpreter cannot see,
///   so the inputs of such a regression cannot be established and Repro rejects it.
///
/// Python itself reports what the regression imported and read, however the import or the read
/// was written, and Repro freezes the non-production ones with the test. The exit status is
/// pytest's.
const PYTEST_RECORDER: &str = "import os, sys, pytest
cwd = os.getcwd()
reads = set()
session_reads = set()
session_started = False
spawns = []
unresolved = []
active = True
SPAWNS = ('subprocess.Popen', 'os.system', 'os.exec', 'os.posix_spawn', 'os.spawn', 'os.fork', 'os.forkpty', 'os.startfile')
def lyra_path(path):
    if isinstance(path, bytes):
        path = os.fsdecode(path)
    if isinstance(path, os.PathLike):
        path = os.fspath(path)
    if not isinstance(path, str):
        return None
    if os.path.isabs(path):
        return path
    try:
        return os.path.join(os.getcwd(), path)
    except OSError:
        unresolved.append(path)
        return None
def lyra_existing(path):
    try:
        return path is not None and os.path.isfile(path)
    except (OSError, ValueError):
        return False
def lyra_audit(event, args):
    if not active:
        return
    if event == 'open':
        path, mode, flags = args
        if mode is None:
            readable = (flags & (os.O_WRONLY | os.O_RDWR)) != os.O_WRONLY
            truncating = bool(flags & getattr(os, 'O_TRUNC', 0))
        else:
            readable = 'r' in mode or '+' in mode
            truncating = 'w' in mode
        path = lyra_path(path)
        if readable and not truncating and lyra_existing(path):
            reads.add(path)
            if session_started:
                session_reads.add(path)
    elif event == 'sqlite3.connect':
        path = lyra_path(args[0]) if args else None
        if lyra_existing(path):
            reads.add(path)
            if session_started:
                session_reads.add(path)
    elif event in SPAWNS:
        what = args[0] if args else ''
        if isinstance(what, (list, tuple)):
            what = what[0] if what else ''
        if isinstance(what, bytes):
            what = os.fsdecode(what)
        spawns.append(event + ' ' + str(what).replace(chr(10), ' ')[:200])
class LyraSession:
    def pytest_sessionstart(self, session):
        global session_started
        session_started = True
sys.addaudithook(lyra_audit)
code = int(pytest.main(sys.argv[1:], plugins=[LyraSession()]))
active = False
root = os.path.realpath(cwd)
def inside(path):
    path = os.path.realpath(path)
    return os.path.relpath(path, root).replace(os.sep, '/') if path.startswith(root + os.sep) else None
for module in list(sys.modules.values()):
    path = getattr(module, '__file__', None)
    if isinstance(path, str) and inside(path):
        print('LYRA-LOADED ' + inside(path))
ROOT_SETUP = ('pyproject.toml', 'setup.cfg', 'tox.ini', 'setup.py')
for path in sorted(reads):
    if os.path.isfile(path) and inside(path):
        if inside(path) in ROOT_SETUP and path not in session_reads:
            continue
        print('LYRA-READ ' + inside(path))
for spawn in spawns:
    print('LYRA-SPAWN ' + spawn)
for path in unresolved:
    print('LYRA-UNRESOLVED ' + path.replace(chr(10), ' ')[:200])
print('LYRA-LOADED-END')
sys.exit(code)";
const LOADED_PREFIX: &str = "LYRA-LOADED ";
const READ_PREFIX: &str = "LYRA-READ ";
const SPAWN_PREFIX: &str = "LYRA-SPAWN ";
const UNRESOLVED_PREFIX: &str = "LYRA-UNRESOLVED ";
const LOADED_END: &str = "LYRA-LOADED-END";

const PYTEST_CONFIG_FILES: [&str; 4] = ["pytest.ini", ".pytest.ini", "pytest.toml", ".pytest.toml"];

/// Directory names that are never source (caches and installed-package folders). Virtual
/// environments and build output are recognised by markers in [`is_generated_python_dir`],
/// so `src/env/` or `src/build/` stay visible.
const GENERATED_DIRS: [&str; 3] = ["__pycache__", "node_modules", "site-packages"];

/// Verification provenance: every project name (one per line on stdin) must be found inside
/// the checkout under test (the working directory). The name is located on the import path
/// without importing it, so no project code runs: `PathFinder` first, which searches
/// `sys.path` exactly as an import would and ignores modules the interpreter already loaded at
/// start-up (a project `types.py` is not mistaken for the standard library's), then the other
/// finders (editable installs). A regular package or module must come from the checkout; a
/// namespace package's first portion, which wins for every subpackage it holds, must be the
/// checkout's (other portions installed by dependencies, as in `google.*`, are allowed). A name
/// that is missing or resolves elsewhere (an editable install of another clone, a regular
/// package shadowing a namespace package) fails, by name, with exit status 1.
const PROVENANCE_CHECK: &str = "import importlib.machinery, importlib.util, os, sys
root = os.path.realpath(os.getcwd())
def inside(path):
    path = os.path.realpath(path)
    return path == root or path.startswith(root + os.sep)
bad = []
for name in [line.strip() for line in sys.stdin if line.strip()]:
    try:
        spec = importlib.machinery.PathFinder.find_spec(name) or importlib.util.find_spec(name)
    except (ImportError, ValueError):
        spec = None
    if spec is not None and spec.origin not in (None, 'namespace', 'built-in', 'frozen'):
        places = [spec.origin]
    else:
        places = list(getattr(spec, 'submodule_search_locations', None) or [])
    print(name, '->', ', '.join(os.path.realpath(p) for p in places) or 'not found')
    if not places or not inside(places[0]):
        bad.append(name)
for name in bad:
    print('ERROR: ' + name + ' does not resolve inside the checkout under test')
sys.exit(1 if bad else 0)";

pub struct PythonPytest;

/// Pytest evidence at the worktree root. Files are read, nothing is executed.
pub fn detect(worktree: &Path) -> Option<ResolvedProfile> {
    let read = |name: &str| {
        let path = worktree.join(name);
        path.is_file().then(|| std::fs::read_to_string(path).unwrap_or_default())
    };
    let mut evidence: Vec<String> = Vec::new();
    for name in PYTEST_CONFIG_FILES {
        if worktree.join(name).is_file() {
            evidence.push(name.into());
        }
    }
    for (file, is_pytest_section) in pytest_section_rules() {
        if read(file).is_some_and(|text| ini_sections(&text).iter().any(|(header, _)| is_pytest_section(header))) {
            evidence.push(file.into());
        }
    }
    if worktree.join("conftest.py").is_file() {
        evidence.push("conftest.py".into());
    }
    if evidence.is_empty() {
        // Declared as a dependency, the weakest evidence: only consulted without configuration.
        if read("pyproject.toml").is_some_and(|text| declares_pytest(&text)) {
            evidence.push("pyproject.toml".into());
        }
        let mut requirements: Vec<String> = std::fs::read_dir(worktree)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".txt") && n.contains("requirements"))
            .collect();
        requirements.sort();
        evidence.extend(requirements.into_iter().filter(|n| read(n).is_some_and(|t| declares_pytest(&t))));
    }
    (!evidence.is_empty()).then(|| ResolvedProfile {
        id: ID.into(),
        version: super::CONTRACT_VERSION,
        project_dir: String::new(),
        evidence,
    })
}

/// A predicate on an INI/TOML section header (without brackets).
type HeaderRule = fn(&str) -> bool;

/// `(file, header predicate)` for configuration files that hold a pytest section.
fn pytest_section_rules() -> [(&'static str, HeaderRule); 3] {
    [
        ("pyproject.toml", |h| h == "tool.pytest" || h.starts_with("tool.pytest.")),
        ("setup.cfg", |h| h == "tool:pytest"),
        ("tox.ini", |h| h == "pytest"),
    ]
}

/// A requirement line or a quoted dependency string naming pytest itself.
fn declares_pytest(text: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    // The name, then a version specifier, extras, a marker, a direct reference (`@ url`), the
    // old parenthesised form, a comment, a line continuation, or the end.
    let re = RE.get_or_init(|| Regex::new(r#"(?mi)(^\s*|["'])pytest\s*([<>=!~;\[,"'@(#\\]|$)"#).unwrap());
    re.is_match(text)
}

/// Splits an INI or TOML file into `(header, non-blank trimmed lines)`; headers lose their
/// brackets (`[tool.pytest.ini_options]` → `tool.pytest.ini_options`). Text based, so a
/// broken file still compares.
fn ini_sections(text: &str) -> Vec<(String, Vec<String>)> {
    let mut sections = vec![(String::new(), Vec::new())];
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with(';')) {
        if line.starts_with('[') && line.ends_with(']') {
            let header = line.trim_matches(|c| c == '[' || c == ']').trim().to_string();
            sections.push((header, Vec::new()));
        } else if let Some(last) = sections.last_mut() {
            last.1.push(line.to_string());
        }
    }
    sections
}

#[async_trait]
impl ExecutionProfile for PythonPytest {
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
        &[PYTHON]
    }
    fn language(&self) -> &'static str {
        "Python"
    }
    fn prompt_macros(&self) -> &'static str {
        include_str!("../prompts/profiles/python-pytest.md")
    }

    fn is_source_file(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "py" || e == "pyi")
            || path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                self.manifest_names().contains(&n) || (n.ends_with(".txt") && n.contains("requirements"))
            })
    }
    fn source_extensions(&self) -> &'static [&'static str] {
        &["py", "pyi"]
    }
    fn manifest_names(&self) -> &'static [&'static str] {
        &[
            "pyproject.toml",
            "setup.cfg",
            "setup.py",
            "pytest.ini",
            ".pytest.ini",
            "pytest.toml",
            ".pytest.toml",
            "tox.ini",
            "requirements.txt",
            "requirements-dev.txt",
            "requirements-test.txt",
            "dev-requirements.txt",
            "test-requirements.txt",
            "constraints.txt",
        ]
    }
    fn generated_dirs(&self) -> &'static [&'static str] {
        &GENERATED_DIRS
    }
    fn is_generated_dir(&self, relative: &Path, path: &Path) -> bool {
        is_generated_python_dir(relative, path)
    }
    fn is_test_path(&self, path: &str) -> bool {
        is_test_path(path)
    }
    async fn describe_repository(&self, worktree: &Path, timeout: Duration, cancel: &CancellationToken) -> Value {
        let mut description = describe_layout(worktree);
        // Versions come from the interpreter LYRA will run, outside the worktree so the
        // read-only Context stage cannot leave byte-code or caches behind.
        for (key, args) in [("interpreter", vec!["--version"]), ("pytest", vec!["-m", "pytest", "--version"])] {
            let spec = CommandSpec::new(PYTHON, std::env::temp_dir())
                .args(args)
                .scrubbed()
                .env("PYTHONDONTWRITEBYTECODE", "1");
            description[key] = match proc::run(&spec, timeout, cancel).await {
                Ok(out) if out.success() => json!(out.combined().lines().map(str::trim).find(|l| !l.is_empty())),
                Ok(out) => json!({ "error": crate::stages::context::cap(out.combined().trim(), 2000) }),
                Err(e) => json!({ "error": format!("{e:#}") }),
            };
        }
        description
    }

    fn validate_record(&self, rec: &ReproRecord) -> Result<(), String> {
        if rec.registration_file.as_deref().is_some_and(|r| !r.trim().is_empty()) {
            return Err("pytest discovers test modules by name; registration_file must be null".into());
        }
        node_id(&rec.test_file, &rec.test_name).map(|_| ())
    }
    fn regression_command_line(&self, rec: &ReproRecord) -> String {
        regression_line(rec.test_name.trim())
    }
    fn regression_command(&self, rec: &ReproRecord, cwd: &Path) -> Result<CommandSpec, String> {
        let id = node_id(&rec.test_file, &rec.test_name)?;
        Ok(regression_spec(&id, cwd))
    }
    fn discovery(&self, spec: &CommandSpec) -> Discovery {
        // Collect the whole recorded test module: once its source is emptied nothing may be
        // collected from it, so the selected node cannot come from a hook or another file.
        let module = spec.args.last().map(|id| id.split("::").next().unwrap_or(id).to_string()).unwrap_or_default();
        let mut discovery = CommandSpec::new(spec.program.clone(), spec.cwd.clone())
            .args(["-c", DISCOVERY_WRAPPER])
            .args(PYTEST_FLAGS)
            .args(["--collect-only", "-q"])
            .arg(module);
        discovery.env = spec.env.clone();
        discovery.clear_env = spec.clear_env;
        Discovery {
            spec: discovery,
            note: "Discovery collects the whole recorded test module in-process; pytest's \"no tests collected\" exit status (5) is reported as success so that an empty collection is evidence, and every other status is unchanged.",
        }
    }
    fn format_test(&self, _worktree: &Path, _test_file: &Path) -> Option<CommandSpec> {
        // Repositories choose their own formatter (black, ruff, none); the harness does not
        // reformat the agent's test. The configured `fmt` verify step still applies.
        None
    }
    fn classify(&self, output: &str, rec: &ReproRecord) -> RegressionOutcome {
        classify_pytest(output, rec.test_name.trim())
    }
    fn describe_outcome(&self, outcome: &RegressionOutcome, rec: &ReproRecord) -> String {
        describe_test_run(outcome, rec.test_name.trim())
    }
    fn confirm_failure(&self, run: &CommandOutput, rec: &ReproRecord) -> Result<(), String> {
        let output = run.combined();
        let id = rec.test_name.trim();
        let counts = parse_summary(&output);
        let exactly_one_failure = counts.as_ref().is_some_and(|c| {
            c.failed == 1 && c.passed == 0 && c.errors == 0 && c.skipped == 0 && c.xfailed == 0 && c.xpassed == 0
        });
        if run.exit_code != Some(1) || !exactly_one_failure || !has_status(&output, "FAILED", id) {
            return Err(
                "reproduction must execute exactly one failing test and exit with pytest's test-failure status (1)"
                    .into(),
            );
        }
        Ok(())
    }
    fn assert_passed(&self, output: &str, rec: &ReproRecord) -> Result<(), String> {
        assert_test_passed(output, rec.test_name.trim())
    }
    fn loaded_test_inputs(&self, output: &str, rec: &ReproRecord, worktree: &Path) -> Result<Vec<String>, String> {
        let read = |p: &str| std::fs::read_to_string(worktree.join(p)).ok();
        parse_loaded_modules(output, &guards::norm_path(&rec.test_file), &ProductionCode::declared(&read))
    }
    fn assert_absent_from_discovery(&self, output: &str, rec: &ReproRecord) -> Result<(), String> {
        assert_absent_from_collection(output, &guards::norm_path(&rec.test_file), rec.test_name.trim())
    }

    fn check_repro_changes(
        &self,
        changes: &[(String, String)],
        rec: &ReproRecord,
        _read_old: FileReader<'_>,
        read_new: FileReader<'_>,
    ) -> Result<(), String> {
        check_repro_changes_with(changes, rec, read_new)
    }
    fn check_fix_changes(
        &self,
        changes: &[(String, String)],
        rec: &ReproRecord,
        read_checkpoint: FileReader<'_>,
        read_now: FileReader<'_>,
    ) -> Result<(), String> {
        check_fix_changes_with(changes, rec, read_checkpoint, read_now)
    }

    fn reproduction_environment(&self, spec: CommandSpec, _run_root: &Path) -> CommandSpec {
        controlled_python_environment(spec, None)
    }
    fn check_environment_root(&self, checkout: &Path) -> Result<(), String> {
        ensure_no_ancestor_pytest_config(checkout)
    }
    fn create_environment(&self, env_root: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(env_root.join("caches"))
    }
    fn check_environment(&self, _env_root: &Path) -> Result<(), String> {
        Ok(())
    }
    fn command_environment(&self, spec: CommandSpec, env_root: &Path) -> CommandSpec {
        controlled_python_environment(spec, Some(&env_root.join("caches")))
    }
    fn preparation(&self, _worktree: &Path, _present: &dyn Fn(&Path) -> bool) -> Option<Preparation> {
        None
    }
    fn verification_steps(
        &self,
        verify: &VerifyCommands,
        rec: &ReproRecord,
        worktree: &Path,
    ) -> Result<Vec<VerifyStep>, String> {
        let line = |name: &str, line: &str| {
            CommandSpec::from_line(line, worktree).map_err(|_| format!("verify step '{name}' has an empty command"))
        };
        let optional =
            |value: &Option<String>| value.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
        let mut steps = Vec::new();
        let names = provenance_names(worktree, &guards::norm_path(&rec.test_file));
        if !names.is_empty() {
            steps.push(VerifyStep {
                name: "imports",
                // Names travel on stdin: no command-line length limit, so none is left unchecked.
                spec: CommandSpec::new(PYTHON, worktree)
                    .args(["-c", PROVENANCE_CHECK])
                    .shown(format!(
                        "{PYTHON} -c <LYRA import-provenance check: {} project names on stdin>",
                        names.len()
                    ))
                    .stdin(names.join("\n") + "\n"),
                log: "50-verify/imports.txt",
                regression: false,
            });
        }
        if let Some(build) = optional(&verify.build) {
            steps.push(VerifyStep {
                name: "build",
                spec: line("build", &build)?,
                log: "50-verify/build.txt",
                regression: false,
            });
        }
        steps.push(VerifyStep {
            name: "repro",
            spec: self.regression_command(rec, worktree)?,
            log: "50-verify/repro-after.txt",
            regression: true,
        });
        // The suite is mandatory: an explicitly empty command is an error, not a skip.
        let tests = verify.test.clone().unwrap_or_else(|| format!("{PYTHON} -m pytest"));
        steps.push(VerifyStep {
            name: "tests",
            spec: line("tests", &tests)?,
            log: "50-verify/tests.txt",
            regression: false,
        });
        if let Some(fmt) = optional(&verify.fmt) {
            steps.push(VerifyStep {
                name: "fmt",
                spec: line("fmt", &fmt)?,
                log: "50-verify/fmt.txt",
                regression: false,
            });
        }
        Ok(steps)
    }
    fn explain_failure(&self, output: &str, exit_code: Option<i32>) -> String {
        first_error_line(output, exit_code)
    }
}

/// Generated output, by marker rather than by name: caches and installed-package folders, any
/// virtual environment (it has `pyvenv.cfg`, whatever it is called), `*.egg-info`, and at the
/// repository root a setuptools `build/` (`lib`, `bdist.*`, `temp.*` inside) or a `dist/` that
/// holds only archives. A package directory that merely shares one of those names is source.
pub fn is_generated_python_dir(relative: &Path, path: &Path) -> bool {
    let Some(name) = relative.file_name().and_then(|n| n.to_str()) else { return false };
    if GENERATED_DIRS.contains(&name) || name.ends_with(".egg-info") {
        return true;
    }
    if path.join("pyvenv.cfg").is_file() {
        return true;
    }
    if relative.components().count() != 1 || path.join("__init__.py").exists() {
        return false;
    }
    let children: Vec<String> = std::fs::read_dir(path)
        .map(|entries| entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    match name {
        "build" => children.iter().any(|c| c == "lib" || c.starts_with("bdist.") || c.starts_with("temp.")),
        "dist" => children.iter().all(|c| [".whl", ".tar.gz", ".zip", ".egg"].iter().any(|ext| c.ends_with(ext))),
        _ => false,
    }
}

/// Every name the provenance step checks: the project's top-level packages and modules
/// ([`importable_names`]) plus each top-level name the frozen regression imports, by an import
/// statement (string literals are not imports), that exists as project code at the top of one
/// of the checkout's import search paths: the root, `src/`, the directories of declared
/// packages (`lib/` for `where = ["lib"]`) and pytest's `pythonpath`. A standard-library or
/// third-party import, or a submodule that merely shares a name deeper in the tree, is not.
pub fn provenance_names(root: &Path, test_file: &str) -> Vec<String> {
    let mut names = importable_names(root);
    if let Ok(source) = std::fs::read_to_string(root.join(test_file)) {
        let read = |p: &str| std::fs::read_to_string(root.join(p)).ok();
        let search = search_paths(&read);
        for module in import_statement_modules(&source) {
            let Some(top) = module.split('.').next().filter(|t| identifier_re().is_match(t)) else { continue };
            if search.iter().any(|dir| project_code_at(&root.join(dir), top)) {
                names.push(top.to_string());
            }
        }
    }
    names.retain(|n| n != "__main__");
    names.sort();
    names.dedup();
    names
}

/// The checkout's import search paths, repository-relative: the root, `src/`, every directory
/// that holds declared packages, and the entries of pytest's `pythonpath` setting.
fn search_paths(read: FileReader<'_>) -> BTreeSet<String> {
    let mut dirs: BTreeSet<String> = ["".to_string(), "src".to_string()].into_iter().collect();
    dirs.extend(ProductionCode::declared(read).roots.into_iter().map(|(dir, _)| dir));
    dirs.extend(pytest_pythonpath(read));
    dirs
}

/// pytest's `pythonpath` entries from `pytest.ini`, `pytest.toml`, `[tool.pytest.ini_options]`
/// in `pyproject.toml`, `[tool:pytest]` in `setup.cfg` or `[pytest]` in `tox.ini`.
fn pytest_pythonpath(read: FileReader<'_>) -> Vec<String> {
    let mut entries = Vec::new();
    if let Some(doc) = read("pyproject.toml").and_then(|t| t.parse::<toml::Value>().ok()) {
        let value = doc.get("tool").and_then(|t| t.get("pytest")).and_then(|p| p.get("ini_options"));
        match value.and_then(|o| o.get("pythonpath")) {
            Some(toml::Value::String(s)) => entries.extend(s.split_whitespace().map(String::from)),
            other => entries.extend(strings_of(other)),
        }
    }
    for (file, section) in
        [("pytest.ini", "pytest"), (".pytest.ini", "pytest"), ("setup.cfg", "tool:pytest"), ("tox.ini", "pytest")]
    {
        let Some(text) = read(file) else { continue };
        for (key, values) in setup_cfg_values(&text) {
            if key.0 == section && key.1 == "pythonpath" {
                entries.extend(values.iter().flat_map(|v| v.split_whitespace()).map(String::from));
            }
        }
    }
    entries.iter().map(|e| join_dir(e, "")).collect()
}

/// Whether `dir` holds project code called `name` at its top: `name.py`, or a directory
/// `name/` with Python source (a package or a namespace portion).
fn project_code_at(dir: &Path, name: &str) -> bool {
    dir.join(format!("{name}.py")).is_file() || (dir.join(name).is_dir() && contains_python(&dir.join(name), 0))
}

/// Whether a directory holds Python source at any depth (up to eight levels, skipping hidden
/// and generated directories), which is what makes a directory without `__init__.py` a PEP 420
/// namespace package: `src/acme/widget/mod.py` makes `acme` importable even though `src/acme/`
/// itself holds no `.py` file.
fn contains_python(dir: &Path, depth: usize) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        match entry.file_type() {
            Ok(kind) if kind.is_file() && name.ends_with(".py") => return true,
            Ok(kind) if kind.is_dir() && !name.starts_with('.') && name != "__pycache__" => {
                let path = entry.path();
                if !path.join("pyvenv.cfg").exists() {
                    subdirs.push(path);
                }
            }
            _ => {}
        }
    }
    depth < 8 && subdirs.iter().any(|d| contains_python(d, depth + 1))
}

/// The project's own top-level packages and modules in the repository root and `src/`, which
/// the verification provenance step requires to resolve inside the checkout: regular packages
/// and modules in both, and PEP 420 namespace packages (a directory without `__init__.py` that
/// holds Python files at any depth, so a namespace root of subpackages counts) under `src/`.
/// Tests, docs, tooling scripts, virtual environments and generated build output (by marker,
/// see [`is_generated_python_dir`]) are not project code.
/// There is no limit on the number of names.
pub fn importable_names(root: &Path) -> Vec<String> {
    const NOT_PROJECT_DIRS: [&str; 8] =
        ["tests", "test", "testing", "docs", "doc", "examples", "scripts", "benchmarks"];
    const NOT_PROJECT_MODULES: [&str; 8] =
        ["setup", "conftest", "noxfile", "fabfile", "tasks", "manage", "sitecustomize", "__main__"];
    let mut names = Vec::new();
    for base in [root.to_path_buf(), root.join("src")] {
        let Ok(entries) = std::fs::read_dir(&base) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else { continue };
            let Ok(kind) = entry.file_type() else { continue };
            let candidate = if kind.is_dir() {
                let package = path.join("__init__.py").is_file() || (base != root && contains_python(&path, 0));
                // Generated output is recognised by its markers, not its name: `src/build/` or a
                // root `build/` package is source.
                let relative = path.strip_prefix(root).unwrap_or(&path);
                (!NOT_PROJECT_DIRS.contains(&file_name.as_str())
                    && !is_generated_python_dir(relative, &path)
                    && package)
                    .then_some(file_name)
            } else if kind.is_file() {
                file_name
                    .strip_suffix(".py")
                    .filter(|stem| {
                        !NOT_PROJECT_MODULES.contains(stem) && !stem.starts_with("test_") && !stem.ends_with("_test")
                    })
                    .map(str::to_owned)
            } else {
                None
            };
            if let Some(name) = candidate.filter(|n| identifier_re().is_match(n)) {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn import_keyword() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(?:from\s+\.*[\w.]*\s+import|import)\s").unwrap())
}

/// Module names a Python source imports: `import a.b, c as d`, `from a.b import c, d`,
/// relative `from . import x` / `from ..m import y` (kept with their dots), parenthesised and
/// backslash-continued lists, imports inside functions, and dotted names in string literals
/// (`importlib.import_module("tests.test_helpers")`). For `from m import n` both `m` and `m.n`
/// are returned, because `n` may be a submodule. Over-approximation is intended: the result
/// only decides what is locked or checked.
pub fn imported_modules(source: &str) -> Vec<String> {
    static STRING: OnceLock<Regex> = OnceLock::new();
    let string_re = STRING.get_or_init(|| Regex::new(r#"["']([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)["']"#).unwrap());
    let mut modules = import_statement_modules(source);
    modules.extend(string_re.captures_iter(source).map(|c| c[1].to_string()));
    modules.sort();
    modules.dedup();
    modules
}

/// The modules named by import statements only (see [`imported_modules`], which also counts
/// dotted names in string literals).
fn import_statement_modules(source: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut depth = 0i64;
    for raw in source.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if current.is_empty() && !import_keyword().is_match(line) {
            continue;
        }
        current.push(' ');
        current.push_str(line.trim_end_matches('\\'));
        depth += line.matches('(').count() as i64 - line.matches(')').count() as i64;
        if depth <= 0 && !line.ends_with('\\') {
            statements.push(std::mem::take(&mut current));
            depth = 0;
        }
    }
    let mut modules = Vec::new();
    let valid =
        |name: &str| !name.is_empty() && name.chars().all(|c| c == '.' || c == '_' || c.is_ascii_alphanumeric());
    for statement in statements {
        let statement = statement.replace(['(', ')'], " ");
        // Simple statements may follow `;` or a compound-statement header on the same line
        // (`if cond: from m import x`, `setup(); import m`), so every segment is examined from
        // its first import keyword.
        for part in statement.split([';', ':']).map(str::trim) {
            let Some(start) = import_keyword().find(part).map(|m| m.start()) else { continue };
            let part = &part[start..];
            if let Some(rest) = part.strip_prefix("import ") {
                modules.extend(rest.split(',').filter_map(|n| n.split_whitespace().next()).map(str::to_owned));
            } else if let Some((module, names)) = part.strip_prefix("from ").and_then(|r| r.split_once(" import ")) {
                let module = module.trim();
                if !module.chars().all(|c| c == '.') {
                    modules.push(module.to_string());
                }
                for name in names.split(',').filter_map(|n| n.split_whitespace().next()).filter(|n| *n != "*") {
                    let joined =
                        if module.ends_with('.') { format!("{module}{name}") } else { format!("{module}.{name}") };
                    modules.push(joined);
                }
            }
        }
    }
    modules.retain(|m| valid(m));
    modules.sort();
    modules.dedup();
    modules
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

fn join_rel(dir: &str, rest: &str) -> String {
    if dir.is_empty() {
        rest.to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

/// Repository files that importing `module` from `importer` may execute: the module file or
/// package `__init__.py`, and the `__init__.py` of every package on the way. Absolute names are
/// looked up where pytest and Python put them on `sys.path`: the importer's base directory (the
/// first ancestor without `__init__.py`, pytest's default import mode), the repository root
/// and `src/`; relative names from the importer's package.
fn module_files(module: &str, importer: &str, exists: &dyn Fn(&str) -> bool) -> Vec<String> {
    let (bases, dotted) = if module.starts_with('.') {
        let dots = module.chars().take_while(|c| *c == '.').count();
        let mut dir = parent_dir(importer);
        for _ in 1..dots {
            dir = parent_dir(dir);
        }
        (vec![dir.to_string()], &module[dots..])
    } else {
        let mut base = parent_dir(importer);
        while !base.is_empty() && exists(&join_rel(base, "__init__.py")) {
            base = parent_dir(base);
        }
        (vec![base.to_string(), String::new(), "src".to_string()], module)
    };
    let parts: Vec<&str> = dotted.split('.').filter(|p| !p.is_empty()).collect();
    let mut files = Vec::new();
    for base in bases {
        for i in 1..=parts.len() {
            let package = join_rel(&base, &parts[..i].join("/"));
            files.push(join_rel(&package, "__init__.py"));
            if i == parts.len() {
                files.push(format!("{package}.py"));
            }
        }
    }
    files.retain(|f| exists(f));
    files.sort();
    files.dedup();
    files
}

/// Files the frozen regression loads that are not production code, transitively: whatever
/// its imports (and those of the files found) resolve to inside the test tree, to a
/// test-named module or `conftest.py` anywhere, or to test support outside the declared
/// production code ([`ProductionCode`]). Production modules it imports are not followed: they
/// are what the fix is meant to change. A module that cannot be classified is an error.
pub fn regression_imports(
    test_file: &str,
    read: FileReader<'_>,
    production: &ProductionCode,
) -> Result<BTreeSet<String>, String> {
    const LIMIT: usize = 2000;
    let tree = test_tree(test_file);
    let exists = |p: &str| read(p).is_some();
    let mut locked = BTreeSet::new();
    let mut unknown = BTreeSet::new();
    let mut queue = vec![test_file.to_string()];
    while let Some(file) = queue.pop() {
        let Some(source) = read(&file) else { continue };
        for module in imported_modules(&source) {
            for dependency in module_files(&module, &file, &exists) {
                if dependency == test_file {
                    continue;
                }
                match production.classify(&dependency, &tree) {
                    Classification::Production => {}
                    Classification::Unknown => {
                        unknown.insert(dependency);
                    }
                    Classification::TestSupport if locked.insert(dependency.clone()) => {
                        if locked.len() > LIMIT {
                            return Err(format!(
                                "the frozen regression reaches more than {LIMIT} test-side modules; LYRA cannot establish its inputs"
                            ));
                        }
                        queue.push(dependency);
                    }
                    Classification::TestSupport => {}
                }
            }
        }
    }
    if !unknown.is_empty() {
        return Err(production.unknown_message(&unknown));
    }
    Ok(locked)
}

/// Test-side code: anything in the test tree, a test-named module or a `conftest.py` anywhere.
fn is_test_side(path: &str, tree: &str) -> bool {
    let name = guards::base_name(path);
    (!tree.is_empty() && path.strip_prefix(tree).is_some_and(|r| r.starts_with('/')))
        || name == "conftest.py"
        || (name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py")))
}

/// Parses the recorder's report (see [`PYTEST_RECORDER`]) into the files the regression
/// actually loaded or read that are not production code, repository-relative: test-side
/// modules, test support anywhere else (a shared `support/helpers.py`, a root helper module)
/// and data the run read (a tracked `fixtures/input.json`, wherever it lives). Pytest's own
/// configuration files are covered by the configuration guard, so they are not frozen as
/// data; byte-code and interpreter caches are nobody's input. A run without the end
/// marker (crashed, truncated, or not run through the recorder), a run that started a child
/// process (whose reads the recorder cannot see) and a file the repository's metadata cannot
/// classify are errors: in each case the inputs cannot be established.
pub fn parse_loaded_modules(output: &str, test_file: &str, production: &ProductionCode) -> Result<Vec<String>, String> {
    if !output.lines().any(|l| l.trim() == LOADED_END) {
        return Err("the regression run did not report the modules it loaded, so LYRA cannot establish which test \
                    inputs to lock"
            .into());
    }
    let spawns: Vec<&str> = output.lines().filter_map(|l| l.trim().strip_prefix(SPAWN_PREFIX)).collect();
    if !spawns.is_empty() {
        return Err(format!(
            "the regression started a child process ({}), whose file reads LYRA cannot observe, so it cannot \
             establish which test inputs to lock; reproduce the bug by calling the project's code in-process",
            spawns.iter().map(|s| format!("`{}`", s.trim())).collect::<Vec<_>>().join(", ")
        ));
    }
    let unresolved: Vec<&str> = output.lines().filter_map(|l| l.trim().strip_prefix(UNRESOLVED_PREFIX)).collect();
    if !unresolved.is_empty() {
        return Err(format!(
            "the regression opened {} relative to a working directory that no longer exists, so LYRA cannot establish \
             which files it read",
            unresolved.iter().map(|p| format!("`{}`", p.trim())).collect::<Vec<_>>().join(", ")
        ));
    }
    let tree = test_tree(test_file);
    let mut loaded = Vec::new();
    let mut unknown = BTreeSet::new();
    let reported = output.lines().map(str::trim).filter_map(|l| {
        l.strip_prefix(LOADED_PREFIX).map(|p| (p, false)).or_else(|| l.strip_prefix(READ_PREFIX).map(|p| (p, true)))
    });
    for (path, is_read) in reported.map(|(p, r)| (guards::norm_path(p), r)) {
        if path.is_empty()
            || path
                .split('/')
                .any(|c| c.is_empty() || c == ".." || c.starts_with('.') || c == "site-packages" || c == "__pycache__")
            || path == test_file
            || (is_read && !is_data_input(&path))
        {
            continue;
        }
        match production.classify(&path, &tree) {
            // Data the run read inside production code may be a test input: frozen too.
            Classification::Production if is_read && !is_source_code(&path) => loaded.push(path),
            Classification::Production => {}
            Classification::TestSupport => loaded.push(path),
            Classification::Unknown => {
                unknown.insert(path);
            }
        }
    }
    if !unknown.is_empty() {
        return Err(production.unknown_message(&unknown));
    }
    loaded.sort();
    loaded.dedup();
    Ok(loaded)
}

/// A file the run read that counts as an input of the regression: anything but byte-code,
/// path files and pytest configuration files (frozen anywhere by the collection guard). The
/// recorder already leaves out pytest's own start-up reads of the root packaging files, so a
/// root `pyproject.toml` that is reported was read by the regression and is frozen whole.
fn is_data_input(path: &str) -> bool {
    let name = guards::base_name(path);
    !(name.ends_with(".pyc") || name.ends_with(".pth")) && !PYTEST_CONFIG_FILES.contains(&name)
}

// ---------------------------------------------------------------------------------------
// Production code against test support
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Classification {
    /// What the fix is meant to change.
    Production,
    /// A regression input, frozen with the test.
    TestSupport,
    /// The repository declares no production code, so the two cannot be told apart.
    Unknown,
}

/// Where the repository says its production code lives, so that everything else the frozen
/// regression loads can be frozen as test support: everything under `src/`, plus the packages
/// and modules its packaging metadata declares, resolved to their actual locations in the
/// checkout:
///
/// - `pyproject.toml`: setuptools `packages`, `py-modules`, `package-dir` (the `""` base and
///   per-package directories) and `packages.find` (`where`, `include` and `exclude`, glob
///   patterns such as `foo*`); poetry `packages` (`include` with its `from` directory); hatch
///   wheel `packages` (paths); flit `module.name`;
/// - `setup.cfg`: `[options] packages` / `py_modules` / `package_dir`, and
///   `[options.packages.find]` `where` / `include` / `exclude`;
/// - `packages = [...]` under `[profiles.python-pytest]` in the repository's `lyra.toml` (names
///   or paths such as `lib/pkg`).
///
/// `packages.find` without `include` declares every package below a `where` other than the
/// root; at the root it declares nothing, and neither does a root pattern made only of
/// wildcards (`include = ["*"]`), because either would make every top-level directory
/// production, test support included. A distribution name (`[project] name`, `[tool.poetry] name`, `[metadata] name`)
/// is not an import name: `published-project` may well ship the package `libcode`. It counts
/// only when a package or module spelled like it exists in the checkout; otherwise the
/// repository has to declare its packages. Read from the frozen checkout, never from the fix.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProductionCode {
    /// Literal top-level import names at the repository root (or under `src/`).
    names: BTreeSet<String>,
    /// `(directory, glob)`: a path is production when its first component below `directory`
    /// (`""` for the root), without `.py`, matches `glob`.
    roots: BTreeSet<(String, String)>,
    /// Exclusions of the same shape (`packages.find` `exclude`).
    excluded: BTreeSet<(String, String)>,
    /// Root-level patterns made only of wildcards (`*`): they would make every top-level
    /// directory production, test support included, so they declare nothing.
    too_broad: BTreeSet<String>,
}

impl ProductionCode {
    pub fn declared(read: FileReader<'_>) -> Self {
        let mut code = Self::default();
        let mut distributions = Vec::new();
        if let Some(text) = read("pyproject.toml") {
            if let Ok(doc) = text.parse::<toml::Value>() {
                let at = |path: &[&str]| path.iter().try_fold(&doc, |v, k| v.get(k));
                let setuptools = at(&["tool", "setuptools"]);
                let package_dir = setuptools.and_then(|s| s.get("package-dir")).and_then(toml::Value::as_table);
                let base = package_dir.and_then(|t| t.get("")).and_then(toml::Value::as_str).unwrap_or("");
                for (name, dir) in package_dir.into_iter().flatten() {
                    if let (false, Some(dir)) = (name.is_empty(), dir.as_str()) {
                        code.add_path(dir, "");
                    }
                }
                let packages = setuptools.and_then(|s| s.get("packages"));
                if let Some(find) = packages.and_then(|p| p.get("find")) {
                    code.add_find(
                        &strings_of(find.get("where")),
                        &strings_of(find.get("include")),
                        &strings_of(find.get("exclude")),
                        base,
                    );
                } else {
                    for raw in strings_of(packages) {
                        code.add_name(&raw, base);
                    }
                }
                for raw in strings_of(setuptools.and_then(|s| s.get("py-modules"))) {
                    code.add_name(&raw, base);
                }
                if let Some(items) = at(&["tool", "poetry", "packages"]).and_then(toml::Value::as_array) {
                    for item in items {
                        match item {
                            toml::Value::String(include) => code.add_name(include, ""),
                            toml::Value::Table(t) => {
                                let from = t.get("from").and_then(toml::Value::as_str).unwrap_or("");
                                if let Some(include) = t.get("include").and_then(toml::Value::as_str) {
                                    code.add_path(include, from);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                for raw in strings_of(at(&["tool", "hatch", "build", "targets", "wheel", "packages"])) {
                    code.add_path(&raw, "");
                }
                for raw in strings_of(at(&["tool", "flit", "module", "name"])) {
                    code.add_name(&raw, "");
                }
                distributions.extend(strings_of(at(&["project", "name"])));
                distributions.extend(strings_of(at(&["tool", "poetry", "name"])));
            }
        }
        if let Some(text) = read("setup.cfg") {
            let sections = setup_cfg_values(&text);
            let get = |section: &str, key: &str| -> Vec<String> {
                sections.get(&(section.to_string(), key.to_string())).cloned().unwrap_or_default()
            };
            distributions.extend(get("metadata", "name"));
            let mut base = String::new();
            for entry in get("options", "package_dir") {
                match entry.split_once('=') {
                    Some((name, dir)) if name.trim().is_empty() => base = dir.trim().to_string(),
                    Some((_, dir)) => code.add_path(dir.trim(), ""),
                    None => {}
                }
            }
            let packages = get("options", "packages");
            if packages.iter().any(|p| p == "find:" || p == "find_namespace:") {
                let find = "options.packages.find";
                code.add_find(&get(find, "where"), &get(find, "include"), &get(find, "exclude"), &base);
            } else {
                for raw in packages {
                    code.add_name(&raw, &base);
                }
            }
            for raw in get("options", "py_modules") {
                code.add_name(&raw, &base);
            }
        }
        if let Some(text) = read("lyra.toml") {
            if let Ok(doc) = text.parse::<toml::Value>() {
                let value = doc.get("profiles").and_then(|p| p.get(ID)).and_then(|p| p.get("packages"));
                for raw in strings_of(value) {
                    code.add_path(&raw, "");
                }
            }
        }
        // A distribution name is only a hint: it names production code when the checkout has a
        // package or module spelled like it (PEP 503 normalisation aside, `My-Project` is
        // imported as `my_project`).
        for raw in distributions {
            let Some(name) = import_name(&raw) else { continue };
            for n in [name.clone(), name.to_ascii_lowercase()] {
                let spelled = [
                    format!("{n}/__init__.py"),
                    format!("{n}.py"),
                    format!("src/{n}/__init__.py"),
                    format!("src/{n}.py"),
                ];
                if spelled.iter().any(|p| read(p).is_some()) {
                    code.names.insert(n);
                }
            }
        }
        code
    }

    /// A dotted package or module name (`pkg`, `pkg.sub`, a glob `pkg*`) below `base`.
    fn add_name(&mut self, raw: &str, base: &str) {
        let raw = raw.trim().trim_matches(|c| c == '"' || c == '\'');
        let Some(top) = raw.split('.').next().filter(|t| is_name_pattern(t)) else { return };
        self.add_root(base, top);
    }

    /// A directory path (`src/hatched`, `lib/pkg`, or a bare `pkg`) below `base`.
    fn add_path(&mut self, raw: &str, base: &str) {
        let raw = raw.trim().trim_matches(|c| c == '"' || c == '\'').trim_end_matches('/');
        let (dir, last) = raw.rsplit_once('/').unwrap_or(("", raw));
        let last = last.strip_suffix(".py").unwrap_or(last);
        if is_name_pattern(last) {
            self.add_root(&join_dir(base, dir), last);
        }
    }

    fn add_find(&mut self, wheres: &[String], includes: &[String], excludes: &[String], base: &str) {
        // `where` is relative to the project root; without it, packages are found in the
        // `package-dir` base.
        let wheres: Vec<String> =
            if wheres.is_empty() { vec![join_dir(base, "")] } else { wheres.iter().map(|w| join_dir(w, "")).collect() };
        for dir in &wheres {
            if includes.is_empty() {
                // Every package in the search path: as broad as `include = ["*"]`.
                self.add_root(dir, "*");
            } else {
                for include in includes {
                    self.add_name(include, dir);
                }
            }
            // An exclusion of a whole top-level package (`tests`, `bench*`) removes it;
            // excluding only subpackages (`pkg.tests`) leaves the package declared.
            for exclude in excludes.iter().map(|e| e.trim()) {
                if !exclude.contains('.') && is_name_pattern(exclude) {
                    self.excluded.insert((dir.clone(), exclude.to_string()));
                }
            }
        }
    }

    fn add_root(&mut self, dir: &str, pattern: &str) {
        let dir = join_dir(dir, "");
        // A pattern made only of wildcards declares every directory in its search path (the
        // root, `src/`, a `pythonpath` such as `lib/`), test support next to the packages
        // included, so it cannot tell production from test support: it declares nothing.
        if pattern.chars().all(|c| c == '*' || c == '?') {
            self.too_broad.insert(if dir.is_empty() { pattern.to_string() } else { format!("{dir}/{pattern}") });
            return;
        }
        if (dir.is_empty() || dir == "src") && identifier_re().is_match(pattern) {
            self.names.insert(pattern.to_string());
        } else {
            self.roots.insert((dir, pattern.to_string()));
        }
    }

    fn is_empty(&self) -> bool {
        self.names.is_empty() && self.roots.is_empty()
    }

    /// The first component of `path` below `dir`, without `.py`.
    fn top_below<'p>(path: &'p str, dir: &str) -> Option<&'p str> {
        let rest = if dir.is_empty() { path } else { path.strip_prefix(dir)?.strip_prefix('/')? };
        let top = rest.split('/').next()?;
        Some(top.strip_suffix(".py").unwrap_or(top))
    }

    fn declares(&self, path: &str) -> bool {
        let matches = |(dir, glob): &(String, String)| Self::top_below(path, dir).is_some_and(|t| glob_match(glob, t));
        if self.excluded.iter().any(matches) {
            return false;
        }
        let named = |dir: &str| Self::top_below(path, dir).is_some_and(|top| self.names.contains(top));
        named("") || named("src") || self.roots.iter().any(matches)
    }

    /// The declarations, for messages: `calc`, `lib/pkg`, `foo*`.
    fn describe(&self) -> String {
        let roots =
            self.roots.iter().map(|(dir, glob)| if dir.is_empty() { glob.clone() } else { format!("{dir}/{glob}") });
        self.names.iter().cloned().chain(roots).map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ")
    }

    /// `path` is repository-relative; `tree` is the regression's test tree.
    fn classify(&self, path: &str, tree: &str) -> Classification {
        if is_test_infrastructure(path, tree) {
            return Classification::TestSupport;
        }
        if self.is_empty() {
            // Only a Python module might be undeclared production code; data and other files
            // are not production code either way.
            let module = [".py", ".pyc", ".pyd", ".so", ".pyw"].iter().any(|ext| path.ends_with(ext));
            return if module { Classification::Unknown } else { Classification::TestSupport };
        }
        if self.declares(path) {
            Classification::Production
        } else {
            Classification::TestSupport
        }
    }

    fn unknown_message(&self, paths: &BTreeSet<String>) -> String {
        format!(
            "the frozen regression loads or reads {}, which LYRA cannot tell apart from production code because the \
             repository declares no packages; declare them (`[tool.setuptools] packages` or `py-modules` in \
             pyproject.toml, `[options] packages` in setup.cfg, or `packages = [...]` under `[profiles.{ID}]` in the \
             repository's lyra.toml; a `[project] name` counts only when a package or module spelled like it exists, \
             at the root or under `src/`): everything the regression loads outside them is then frozen as test support",
            paths.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ")
        ) + &if self.too_broad.is_empty() {
            String::new()
        } else {
            format!(
                " (the declared pattern {} matches every package in its search path, test support next to them \
                 included, so it was ignored; name the packages)",
                self.too_broad.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ")
            )
        }
    }
}

/// `base/dir` without `.` components or surrounding slashes (`""` is the repository root).
fn join_dir(base: &str, dir: &str) -> String {
    base.split('/')
        .chain(dir.split('/'))
        .map(str::trim)
        .filter(|c| !c.is_empty() && *c != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// An identifier, optionally with `*` / `?` glob characters.
fn is_name_pattern(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '*' | '?'))
}

/// `fnmatch`-style matching with `*` and `?`, iterative: only the last `*` is ever
/// backtracked to, so the work is bounded by `pattern.len() * text.len()` whatever the
/// pattern (no exponential case for repeated stars).
fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((star_pi, star_ti)) = star {
            pi = star_pi + 1;
            ti = star_ti + 1;
            star = Some((star_pi, star_ti + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// `setup.cfg` values by `(section, key)`: comma- and line-separated items, with indented
/// continuation lines (and the `=dir` / `pkg = dir` lines of `package_dir`) belonging to the
/// key above them.
fn setup_cfg_values(text: &str) -> HashMap<(String, String), Vec<String>> {
    let mut values: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut section = String::new();
    let mut key: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.trim_matches(|c| c == '[' || c == ']').trim().to_string();
            key = None;
            continue;
        }
        let items = if raw.starts_with([' ', '\t']) {
            line.to_string()
        } else if let Some((k, v)) = line.split_once(['=', ':']).filter(|(k, _)| !k.trim().is_empty()) {
            key = Some(k.trim().to_string());
            v.trim().to_string()
        } else {
            line.to_string()
        };
        let Some(key) = key.clone() else { continue };
        let entry = values.entry((section.clone(), key.clone())).or_default();
        if key == "package_dir" {
            if !items.is_empty() {
                entry.push(items);
            }
        } else {
            entry.extend(items.split(',').map(str::trim).filter(|i| !i.is_empty()).map(String::from));
        }
    }
    values
}

/// The strings in a TOML value: a string, an array of strings, or the `include` keys of an
/// array of tables.
fn strings_of(value: Option<&toml::Value>) -> Vec<String> {
    match value {
        Some(toml::Value::String(s)) => vec![s.clone()],
        Some(toml::Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                toml::Value::String(s) => Some(s.clone()),
                toml::Value::Table(t) => t.get("include").and_then(|v| v.as_str()).map(String::from),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The top-level import name a distribution name refers to: `my-project` → `my_project`;
/// non-identifiers are skipped.
fn import_name(raw: &str) -> Option<String> {
    let last = raw.trim().trim_matches('"').trim_matches('\'').trim_end_matches('/').rsplit('/').next()?;
    let name = last.split(['.', '*']).next()?.replace('-', "_");
    identifier_re().is_match(&name).then_some(name)
}

/// Source code a fix may change inside production code. Anything else there (package data,
/// JSON, CSV, templates) may be a test input the regression reads, which LYRA cannot tell
/// apart from production data, so it stays frozen.
fn is_source_code(path: &str) -> bool {
    const SOURCE: [&str; 13] =
        [".py", ".pyi", ".pyw", ".pyx", ".pxd", ".pxi", ".c", ".h", ".cc", ".cpp", ".cxx", ".hpp", ".hh"];
    SOURCE.iter().any(|ext| path.ends_with(ext))
}

// ---------------------------------------------------------------------------------------
// Repository description (10-context/repo.json)
// ---------------------------------------------------------------------------------------

/// Configuration files, `testpaths` and test directories, read without executing anything.
fn describe_layout(worktree: &Path) -> Value {
    let read = |name: &str| std::fs::read_to_string(worktree.join(name)).ok();
    let mut config_files = Vec::new();
    let mut testpaths: Vec<String> = Vec::new();
    for name in
        PYTEST_CONFIG_FILES.iter().chain(["pyproject.toml", "setup.cfg", "tox.ini", "setup.py", "conftest.py"].iter())
    {
        let Some(text) = read(name) else { continue };
        config_files.push(*name);
        for (_, lines) in ini_sections(&text) {
            for line in lines {
                if let Some(rest) =
                    line.strip_prefix("testpaths").map(str::trim_start).and_then(|r| r.strip_prefix('='))
                {
                    testpaths.extend(
                        rest.split(|c: char| c == ',' || c.is_whitespace() || c == '[' || c == ']')
                            .map(|p| p.trim_matches(|c| c == '"' || c == '\''))
                            .filter(|p| !p.is_empty())
                            .map(str::to_owned),
                    );
                }
            }
        }
    }
    let test_dirs: Vec<&str> = ["tests", "test", "testing"].into_iter().filter(|d| worktree.join(d).is_dir()).collect();
    json!({
        "profile": ID,
        "project_root": "",
        "config_files": config_files,
        "testpaths": testpaths,
        "test_dirs": test_dirs,
        "regression_command": format!("{PYTHON} -m pytest {} <node id>", PYTEST_FLAGS.join(" ")),
    })
}

// ---------------------------------------------------------------------------------------
// Regression identity and command
// ---------------------------------------------------------------------------------------

/// The regression as it runs: pytest in-process through the import recorder (what source
/// binding, the fix lock and verification freeze), shown to people as the equivalent plain
/// pytest command ([`regression_line`]).
fn regression_spec(id: &str, cwd: &Path) -> CommandSpec {
    CommandSpec::new(PYTHON, cwd).args(["-c", PYTEST_RECORDER]).args(PYTEST_FLAGS).arg(id).shown(regression_line(id))
}

/// The one-line reproduction command recorded in `repro.json` and printed in the draft PR:
/// the same pytest flags and node id without the recorder. It is copyable as typed in sh and
/// PowerShell: the flags carry no quotes or spaces and the node id is validated to plain path
/// characters, `::` and identifiers.
fn regression_line(id: &str) -> String {
    format!("{PYTHON} -m pytest {} {id}", PYTEST_FLAGS.join(" "))
}

fn identifier_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap())
}

/// A pytest test module: a repository-relative `.py` path named `test_*.py` or `*_test.py`,
/// built from plain path characters with no component that pytest could read as an option.
pub fn validate_test_module(test_file: &str) -> Result<String, String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_./-]+$").unwrap());
    let path = guards::norm_path(test_file);
    let name = guards::base_name(&path);
    if !re.is_match(&path) || path.split('/').any(|c| c.starts_with('-')) {
        return Err(format!("test file {path:?} must be a plain repository-relative path"));
    }
    if name == "conftest.py" || !(name.starts_with("test_") && name.ends_with(".py") || name.ends_with("_test.py")) {
        return Err(format!("test file {path} must be a pytest test module named test_*.py or *_test.py"));
    }
    Ok(path)
}

/// The exact pytest node id: `<test_file>::<function>` or `<test_file>::<Class>::<method>`,
/// with plain identifiers (no parametrize ids, no globbing).
pub fn node_id(test_file: &str, test_name: &str) -> Result<String, String> {
    let module = validate_test_module(test_file)?;
    let id = test_name.trim();
    let rest = id
        .strip_prefix(&format!("{module}::"))
        .ok_or_else(|| format!("test_name must be the pytest node id `{module}::<test function>`, got {id:?}"))?;
    let parts: Vec<&str> = rest.split("::").collect();
    let valid = match parts.as_slice() {
        [function] => identifier_re().is_match(function) && function.starts_with("test"),
        [class, method] => {
            identifier_re().is_match(class)
                && class.starts_with("Test")
                && identifier_re().is_match(method)
                && method.starts_with("test")
        }
        _ => false,
    };
    if !valid {
        return Err(format!(
            "test_name {id:?} must select exactly one test: `{module}::test_<name>` or `{module}::Test<Class>::test_<name>`"
        ));
    }
    Ok(id.to_string())
}

// ---------------------------------------------------------------------------------------
// Output parsing
// ---------------------------------------------------------------------------------------

/// Counts from pytest's final summary line (`==== 1 failed, 2 passed in 0.05s ====`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Counts {
    pub passed: u64,
    pub failed: u64,
    pub errors: u64,
    pub skipped: u64,
    pub xfailed: u64,
    pub xpassed: u64,
    pub deselected: u64,
}

impl Counts {
    fn total(&self) -> u64 {
        self.passed + self.failed + self.errors + self.skipped + self.xfailed + self.xpassed
    }
}

fn strip_banner(line: &str) -> &str {
    line.trim().trim_matches('=').trim()
}

/// The last pytest summary line in the output, or `None` when there is none (a crashed,
/// truncated or non-pytest run). Unknown words make the line unrecognised, never zero.
pub fn parse_summary(output: &str) -> Option<Counts> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^(.+?) in [0-9.]+s(?: \([^)]*\))?$").unwrap());
    'lines: for line in output.lines().rev() {
        let Some(captures) = re.captures(strip_banner(line)) else { continue };
        let body = &captures[1];
        if body == "no tests ran" {
            return Some(Counts::default());
        }
        let mut counts = Counts::default();
        for part in body.split(", ") {
            let Some((number, word)) = part.split_once(' ') else { continue 'lines };
            let Ok(n) = number.parse::<u64>() else { continue 'lines };
            match word {
                "passed" => counts.passed += n,
                "failed" => counts.failed += n,
                "error" | "errors" => counts.errors += n,
                "skipped" => counts.skipped += n,
                "xfailed" => counts.xfailed += n,
                "xpassed" => counts.xpassed += n,
                "deselected" => counts.deselected += n,
                "warning" | "warnings" | "rerun" | "reruns" | "subtests passed" => {}
                _ => continue 'lines,
            }
        }
        return Some(counts);
    }
    None
}

/// A `-rA` short-summary line for exactly this node id: `FAILED <id>` or `FAILED <id> - msg`.
fn has_status(output: &str, status: &str, id: &str) -> bool {
    let exact = format!("{status} {id}");
    let with_reason = format!("{exact} - ");
    output.lines().map(str::trim).any(|l| l == exact || l.starts_with(&with_reason))
}

fn missing_pytest(output: &str) -> bool {
    output.contains("No module named pytest")
}

/// Classifies the output of the regression command for node id `id`.
pub fn classify_pytest(output: &str, id: &str) -> RegressionOutcome {
    use RegressionOutcome::*;
    if missing_pytest(output) {
        return Other(format!(
            "pytest is not installed for `{PYTHON}`: start LYRA from an environment that has pytest and the project's dependencies"
        ));
    }
    let Some(counts) = parse_summary(output) else {
        return Other("no pytest summary line in the output".into());
    };
    if counts.errors > 0 {
        return SetupFailed;
    }
    if output
        .lines()
        .any(|l| l.starts_with("ERROR: not found:") || l.starts_with("ERROR: file or directory not found:"))
    {
        return NotDiscovered;
    }
    if counts.failed > 0 {
        return if has_status(output, "FAILED", id) {
            Failed
        } else {
            Other(format!("{} test(s) failed but {id} is not among them", counts.failed))
        };
    }
    if counts.passed > 0 {
        return if has_status(output, "PASSED", id) {
            Passed
        } else {
            Other(format!("{} test(s) passed but {id} is not among them", counts.passed))
        };
    }
    if counts.skipped + counts.xfailed + counts.xpassed > 0 {
        return Skipped;
    }
    if counts.total() == 0 {
        return NotDiscovered;
    }
    Other(format!("test {id} did not appear in the output"))
}

pub fn describe_test_run(run: &RegressionOutcome, id: &str) -> String {
    match run {
        RegressionOutcome::SetupFailed => {
            "the test module could not be collected or a fixture failed (import, syntax or setup error)".into()
        }
        RegressionOutcome::NotDiscovered => {
            format!("test {id} did not run (no tests ran: the node id is wrong or pytest does not collect that module)")
        }
        RegressionOutcome::Passed => format!("test {id} passed"),
        RegressionOutcome::Failed => format!("test {id} FAILED"),
        RegressionOutcome::Skipped => format!("test {id} was skipped or marked xfail/xpass"),
        RegressionOutcome::Other(s) => s.clone(),
    }
}

/// Verify rule (PLAN §3.4 step 2): the node passed and it was the only test that ran.
pub fn assert_test_passed(output: &str, id: &str) -> Result<(), String> {
    match classify_pytest(output, id) {
        RegressionOutcome::Passed => {
            let counts = parse_summary(output).unwrap_or_default();
            if counts.passed == 1 && counts.total() == 1 {
                Ok(())
            } else {
                Err(format!("expected exactly 1 passed test, summary shows {counts:?}"))
            }
        }
        other => Err(describe_test_run(&other, id)),
    }
}

/// Source binding: with the recorded module emptied, collecting it must finish and list no
/// node from it (in particular not the selected one).
pub fn assert_absent_from_collection(output: &str, test_file: &str, id: &str) -> Result<(), String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^(no tests collected|\d+ tests? collected)\b(.*)$").unwrap());
    let module_prefix = format!("{test_file}::");
    let mut saw_summary = false;
    for line in output.lines().map(str::trim) {
        if line == id || line.starts_with(&module_prefix) {
            return Err(format!("test {id} is still collected without the recorded reproduction source"));
        }
        if let Some(captures) = re.captures(strip_banner(line)) {
            saw_summary = true;
            if !captures[1].starts_with("no ") || captures[2].contains("error") {
                return Err(format!("tests are still collected without the recorded reproduction source: {line}"));
            }
        }
    }
    if !saw_summary {
        return Err("source binding check produced no pytest collection summary".into());
    }
    Ok(())
}

/// The line that best explains a failed command: a missing pytest, a `FAILED`/`ERROR`
/// summary line or an assertion line; otherwise the exit code.
fn first_error_line(output: &str, exit_code: Option<i32>) -> String {
    for line in output.lines().map(str::trim) {
        if line.contains("No module named")
            || line.starts_with("FAILED ")
            || line.starts_with("ERROR ")
            || line.starts_with("ERROR:")
            || line.starts_with("E   ")
            || line.starts_with("would reformat")
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
// Paths
// ---------------------------------------------------------------------------------------

/// A path that pytest or convention treats as test code.
pub fn is_test_path(path: &str) -> bool {
    let name = guards::base_name(path);
    guards::has_dir(path, "tests")
        || guards::has_dir(path, "test")
        || name == "conftest.py"
        || (name.starts_with("test_") && name.ends_with(".py"))
        || name.ends_with("_test.py")
}

/// The directory tree of test code around the frozen test: the deepest ancestor named
/// `tests`, `test` or `testing`. A test that lives next to the code it tests
/// (`textkit/test_lists.py`) has none: its directory is production code, so only test-named
/// modules, `conftest.py` and test or fixture directories there count as test infrastructure.
fn test_tree(test_file: &str) -> String {
    let parts: Vec<&str> = test_file.split('/').collect();
    let dirs = &parts[..parts.len().saturating_sub(1)];
    match dirs.iter().rposition(|d| matches!(*d, "tests" | "test" | "testing")) {
        Some(i) => dirs[..=i].join("/"),
        None => String::new(),
    }
}

/// Files that change how pytest collects, imports or reports, wherever they are.
fn collection_control_violation(path: &str) -> Option<&'static str> {
    let name = guards::base_name(path);
    if name == "conftest.py" {
        Some("conftest.py changes collection, fixtures and hooks")
    } else if PYTEST_CONFIG_FILES.contains(&name) {
        Some("pytest configuration must not change")
    } else if name == "sitecustomize.py" || name == "usercustomize.py" || name.ends_with(".pth") {
        Some("interpreter start-up hooks must not change")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------
// Repro diff rules (PLAN §3.1)
// ---------------------------------------------------------------------------------------

/// S2 allowed set: the new test module, new non-Python fixtures under `tests/` or
/// `fixtures/`, and an empty `__init__.py` there. Nothing that changes collection.
pub fn check_repro_changes_with(
    changes: &[(String, String)],
    rec: &ReproRecord,
    read_new: FileReader<'_>,
) -> Result<(), String> {
    let test_file = guards::norm_path(&rec.test_file);
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
        if let Some(reason) = collection_control_violation(&path) {
            violations.push(format!("{status} {path}: {reason}"));
            continue;
        }
        let name = guards::base_name(&path);
        if guards::is_new(status) && (guards::has_dir(&path, "tests") || guards::has_dir(&path, "fixtures")) {
            if name == "__init__.py" && read_new(&path).is_some_and(|t| t.trim().is_empty()) {
                continue;
            }
            if name.ends_with(".py") || name.ends_with(".pyi") {
                violations
                    .push(format!("{status} {path}: only the test module may be new Python code in the repro stage"));
            }
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
// Fix rules (PLAN §3.3): the test is locked
// ---------------------------------------------------------------------------------------

pub fn check_fix_changes_with(
    changes: &[(String, String)],
    rec: &ReproRecord,
    read_checkpoint: FileReader<'_>,
    read_now: FileReader<'_>,
) -> Result<(), String> {
    if changes.is_empty() {
        return Err("fix produced no changes".to_string());
    }
    // 1. The test module and every frozen input are byte-for-byte their frozen form. This
    //    takes precedence over everything below, including inputs under `src/` or a package.
    let mut violations = guards::frozen_test_violations(changes, rec, read_checkpoint, read_now);
    let frozen: BTreeSet<String> =
        std::iter::once(&rec.test_file).chain(&rec.frozen_inputs).map(|p| guards::norm_path(p)).collect();

    // 2. Everything else may change only when it is declared production code (an allowlist,
    //    PLAN §13.6): what the regression reads or runs need not be observed to be protected.
    let test_file = guards::norm_path(&rec.test_file);
    let tree = test_tree(&test_file);
    let production = ProductionCode::declared(read_checkpoint);
    if let Err(reason) = regression_imports(&test_file, read_checkpoint, &production) {
        violations.push(reason);
    }
    for (status, raw) in changes {
        let path = guards::norm_path(raw);
        if frozen.contains(&path) {
            continue; // reported by the frozen-test check above
        }
        if let Some(reason) = fix_may_not_change(&path, status, &production, &tree, read_checkpoint, read_now) {
            violations.push(format!("{status} {path}: {reason}"));
        }
    }

    // 3. The recorded command is our constant.
    let expected = regression_line(rec.test_name.trim());
    if rec.command.trim() != expected {
        violations.push(format!("recorded test command changed: {:?} (expected {expected:?})", rec.command.trim()));
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!("fix violates the test lock: {}", violations.join("; ")))
    }
}

/// Why a fix may not add, modify or delete `path` (a rename is a deletion and an addition, and
/// both halves are checked), or `None` when it may. In order:
///
/// 1. collection controls anywhere (`conftest.py`, pytest configuration files, start-up hooks);
/// 2. the root manifests, whose dependency fields only may change ([`manifest_violation`]);
/// 3. test infrastructure anywhere, also under `src/` or a declared package: test modules,
///    helpers, fixtures and data ([`is_test_infrastructure`]);
/// 4. anything that is not declared production code ([`ProductionCode`]);
/// 5. a new top-level module or package initializer under `src/`, which could shadow the test
///    runner or a dependency on the bound import path.
fn fix_may_not_change(
    path: &str,
    status: &str,
    production: &ProductionCode,
    tree: &str,
    read_checkpoint: FileReader<'_>,
    read_now: FileReader<'_>,
) -> Option<String> {
    if let Some(reason) = collection_control_violation(path) {
        return Some(reason.to_string());
    }
    if is_root_manifest(path) {
        return manifest_violation(path, status, read_checkpoint, read_now);
    }
    if is_test_infrastructure(path, tree) {
        return Some(
            "tests, test helpers and fixtures are frozen with the regression wherever they live, including under \
             `src/` and declared packages"
                .into(),
        );
    }
    match production.classify(path, tree) {
        Classification::Production if guards::is_new(status) && adds_top_level_module(path) => Some(
            "a new top-level module or package under `src/` could shadow the test runner or a dependency; \
             add it inside an existing package"
                .into(),
        ),
        Classification::Production if !is_source_code(path) => Some(
            "only source code may change inside production code: package data (JSON, CSV, text, templates) may be \
             a test input the regression reads, which LYRA cannot tell apart from production data, so it is frozen"
                .into(),
        ),
        Classification::Production => None,
        Classification::Unknown => Some(production.unknown_message(&BTreeSet::from([path.to_string()]))),
        Classification::TestSupport => Some(format!(
            "not declared production code; a fix may change only the declared packages{} and the dependency \
             fields of the root manifests",
            if production.is_empty() { String::new() } else { format!(" ({})", production.describe()) }
        )),
    }
}

/// Test code and data anywhere: everything in the regression's test tree, every test module
/// and `conftest.py`, and every file below a directory named `tests`, `test`, `testing`,
/// `fixtures`, `testdata` or `test_data`, also inside `src/` or a declared package
/// (`src/pkg/tests/fixtures/input.json`).
fn is_test_infrastructure(path: &str, tree: &str) -> bool {
    is_test_side(path, tree)
        || path
            .split('/')
            .rev()
            .skip(1)
            .any(|dir| matches!(dir, "tests" | "test" | "testing" | "fixtures" | "testdata" | "test_data"))
}

/// Manifests whose dependency fields a fix may change: the repository root's
/// `pyproject.toml`, `setup.cfg` and `requirements*.txt`. A file with the same name anywhere
/// else (a `fixtures/pyproject.toml`) is not a manifest.
fn is_root_manifest(path: &str) -> bool {
    !path.contains('/')
        && (path == "pyproject.toml"
            || path == "setup.cfg"
            || (path.starts_with("requirements") && path.ends_with(".txt")))
}

/// A root manifest may only be modified, never added or deleted, and only in its dependency
/// fields: `[project] dependencies` / `optional-dependencies`, `[dependency-groups]` and the
/// poetry dependency tables in `pyproject.toml`; `install_requires` and
/// `[options.extras_require]` in `setup.cfg`; any line of a root `requirements*.txt` (LYRA
/// installs nothing, so these never change what runs). Pytest configuration, plugin entry
/// points, package lists and every other field stay frozen.
fn manifest_violation(
    path: &str,
    status: &str,
    read_checkpoint: FileReader<'_>,
    read_now: FileReader<'_>,
) -> Option<String> {
    let (Some(old), Some(new)) = (read_checkpoint(path), read_now(path)) else {
        return Some("a root manifest may have its dependency fields changed, but not be added or deleted".into());
    };
    if guards::is_new(status) || status == "D" {
        return Some("a root manifest may have its dependency fields changed, but not be added or deleted".into());
    }
    let same =
        match path {
            "pyproject.toml" => match (pyproject_without_dependencies(&old), pyproject_without_dependencies(&new)) {
                (Some(old), Some(new)) => old == new,
                (None, _) => return Some(
                    "the frozen pyproject.toml is not valid TOML, so its dependency fields cannot be told apart from \
                     the rest; it stays frozen"
                        .into(),
                ),
                (Some(_), None) => return Some("the manifest is no longer valid TOML".into()),
            },
            "setup.cfg" => setup_cfg_without_dependencies(&old) == setup_cfg_without_dependencies(&new),
            _ => true,
        };
    (!same).then(|| "only the dependency fields of a root manifest may change".into())
}

fn pyproject_without_dependencies(text: &str) -> Option<toml::Value> {
    let mut doc: toml::Value = text.parse().ok()?;
    let table = doc.as_table_mut()?;
    table.remove("dependency-groups");
    if let Some(project) = table.get_mut("project").and_then(toml::Value::as_table_mut) {
        project.remove("dependencies");
        project.remove("optional-dependencies");
    }
    let poetry = table.get_mut("tool").and_then(|t| t.get_mut("poetry")).and_then(toml::Value::as_table_mut);
    if let Some(poetry) = poetry {
        poetry.remove("dependencies");
        poetry.remove("dev-dependencies");
        if let Some(groups) = poetry.get_mut("group").and_then(toml::Value::as_table_mut) {
            for group in groups.iter_mut().filter_map(|(_, g)| g.as_table_mut()) {
                group.remove("dependencies");
            }
        }
    }
    Some(doc)
}

/// `setup.cfg` as (section, line) pairs without `[options] install_requires` (with its
/// indented continuation lines) and `[options.extras_require]`.
fn setup_cfg_without_dependencies(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut section = String::new();
    let mut skipping = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.trim_matches(|c| c == '[' || c == ']').trim().to_string();
            skipping = false;
            if section != "options.extras_require" {
                out.push((section.clone(), String::new()));
            }
            continue;
        }
        if !raw.starts_with([' ', '\t']) {
            let key = line.split(['=', ':']).next().unwrap_or("").trim();
            skipping = section == "options" && key == "install_requires";
        }
        if !skipping && section != "options.extras_require" {
            out.push((section.clone(), line.to_string()));
        }
    }
    out
}

/// `src/<name>.<ext>` for an importable extension, or `src/<name>/__init__.<ext>`.
fn adds_top_level_module(path: &str) -> bool {
    const IMPORTABLE: [&str; 5] = [".py", ".pyc", ".pyd", ".so", ".pyw"];
    let Some(rest) = path.strip_prefix("src/") else { return false };
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [file] => IMPORTABLE.iter().any(|ext| file.ends_with(ext)),
        [_, init] => init.starts_with("__init__.") && IMPORTABLE.iter().any(|ext| init.ends_with(ext)),
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------
// Environments
// ---------------------------------------------------------------------------------------

/// Scrubbed environment (no PYTHONPATH, PYTHONHOME, PYTEST_ADDOPTS or PYTEST_PLUGINS from
/// the operator), no byte-code written into the checkout, and tool caches that would
/// otherwise land in the working directory redirected to the owned environment.
///
/// Imports are bound to the checkout under test: `PYTHONPATH` lists the command's working
/// directory and its `src/`, which Python searches before site-packages, `.pth` path entries
/// and editable-install finders. An installed copy of the project (for example an editable
/// install of the operator's own clone) therefore cannot stand in for the candidate's code.
fn controlled_python_environment(spec: CommandSpec, caches: Option<&Path>) -> CommandSpec {
    const OWNED: [&str; 5] =
        ["PYTHONDONTWRITEBYTECODE", "PYTEST_ADDOPTS", "RUFF_CACHE_DIR", "MYPY_CACHE_DIR", "PYTHONPATH"];
    let checkout_paths = [spec.cwd.clone(), spec.cwd.join("src")];
    let mut spec = spec.scrubbed();
    spec.env.retain(|(key, _)| !OWNED.iter().any(|owned| key.eq_ignore_ascii_case(owned)));
    if let Ok(path) = std::env::join_paths(checkout_paths.iter().filter(|p| p.is_dir() || *p == &checkout_paths[0])) {
        spec.env.push(("PYTHONPATH".into(), path.to_string_lossy().into_owned()));
    }
    spec.env.push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
    // Applies to the configured suite command too, which the harness does not build.
    spec.env.push(("PYTEST_ADDOPTS".into(), "-p no:cacheprovider".into()));
    if let Some(caches) = caches {
        spec.env.push(("RUFF_CACHE_DIR".into(), caches.join("ruff").to_string_lossy().into_owned()));
        spec.env.push(("MYPY_CACHE_DIR".into(), caches.join("mypy").to_string_lossy().into_owned()));
    }
    spec
}

/// pytest looks for configuration and `conftest.py` in ancestors of the checkout when the
/// repository does not pin them; refuse a verification root below such files.
fn ensure_no_ancestor_pytest_config(checkout: &Path) -> Result<(), String> {
    let names = PYTEST_CONFIG_FILES.iter().chain(&["conftest.py", "pyproject.toml", "setup.cfg", "tox.ini"]);
    for ancestor in checkout.parent().into_iter().flat_map(Path::ancestors) {
        for name in names.clone() {
            let path = ancestor.join(name);
            if std::fs::symlink_metadata(&path).is_ok() {
                return Err(format!(
                    "verification would inherit pytest configuration from {}; move LYRA_HOME outside that directory",
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

    const FILE: &str = "tests/test_issue_7.py";
    const ID_: &str = "tests/test_issue_7.py::test_issue_7_fails";

    // Captured from pytest 9.2.0 / Python 3.14 with the harness flags.
    const FAILED: &str = "\
============================= test session starts ==============================
platform darwin -- Python 3.14.7, pytest-9.2.0, pluggy-1.7.0
rootdir: /work/repo
configfile: pyproject.toml
collected 1 item

tests/test_issue_7.py F                                                  [100%]

=================================== FAILURES ===================================
______________________________ test_issue_7_fails ______________________________

    def test_issue_7_fails():
>       assert 1 + 1 == 3, \"sum must be three\"
E       AssertionError: sum must be three
E       assert (1 + 1) == 3

tests/test_issue_7.py:3: AssertionError
=========================== short test summary info ============================
FAILED tests/test_issue_7.py::test_issue_7_fails - AssertionError: sum must b...
============================== 1 failed in 0.05s ===============================
";
    const PASSED: &str = "\
collected 1 item

tests/test_issue_7.py .                                                  [100%]

==================================== PASSES ====================================
=========================== short test summary info ============================
PASSED tests/test_issue_7.py::test_issue_7_fails
============================== 1 passed in 0.01s ===============================
";
    const SKIPPED: &str = "\
collected 1 item
=========================== short test summary info ============================
SKIPPED [1] tests/test_issue_7.py:6: nope
============================== 1 skipped in 0.01s ==============================
";
    const XFAILED: &str = "\
=========================== short test summary info ============================
XFAIL tests/test_issue_7.py::test_issue_7_fails
============================== 1 xfailed in 0.02s ==============================
";
    const NOT_FOUND: &str = "\
ERROR: not found: /work/repo/tests/test_issue_7.py::test_missing
(no match in any of [<Module test_issue_7.py>])

collected 0 items

============================ no tests ran in 0.01s =============================
";
    const COLLECTION_ERROR: &str = "\
ERROR: found no collectors for /work/repo/tests/test_issue_7.py::test_issue_7_fails

collected 0 items / 1 error

____________________ ERROR collecting tests/test_issue_7.py _____________________
E   ModuleNotFoundError: No module named 'nosuchmod'
=========================== short test summary info ============================
ERROR tests/test_issue_7.py
=============================== 1 error in 0.03s ===============================
";
    const OTHER_FAILED: &str = "\
FAILED tests/test_issue_7.py::test_other - AssertionError: x
============================== 1 failed in 0.05s ===============================
";

    fn rec() -> ReproRecord {
        ReproRecord {
            reproduced: true,
            krate: String::new(),
            test_file: FILE.into(),
            test_name: ID_.into(),
            command: regression_line(ID_),
            expected: "sum must be three".into(),
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

    /// Packaging metadata declaring `calc` as the production code of the fixture repositories:
    /// a distribution name, which counts because the fixtures really have a `calc.py`.
    const PYPROJECT: &str = "[project]\nname = \"calc\"\n";
    fn declared() -> ProductionCode {
        ProductionCode::declared(&|p: &str| match p {
            "pyproject.toml" => Some(PYPROJECT.to_string()),
            "calc.py" => Some(String::new()),
            _ => None,
        })
    }

    fn output(exit: i32, text: &str) -> CommandOutput {
        CommandOutput { exit_code: Some(exit), stdout: text.into(), ..Default::default() }
    }

    #[test]
    fn detection_needs_pytest_evidence_not_just_python_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("setup.py"), "from setuptools import setup\n").unwrap();
        std::fs::create_dir(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("tests/test_a.py"), "def test_a(): pass\n").unwrap();
        assert!(detect(dir.path()).is_none(), "Python files alone do not pick a runner");

        std::fs::write(dir.path().join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        assert!(detect(dir.path()).is_none());
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = \"x\"\n[tool.pytest.ini_options]\naddopts = \"-q\"\n",
        )
        .unwrap();
        let resolved = detect(dir.path()).unwrap();
        assert_eq!((resolved.id.as_str(), resolved.evidence.clone()), (ID, vec!["pyproject.toml".to_string()]));
        assert_eq!(resolved.project_dir, "");

        std::fs::write(dir.path().join("pytest.ini"), "[pytest]\n").unwrap();
        assert_eq!(detect(dir.path()).unwrap().evidence, vec!["pytest.ini", "pyproject.toml"]);
    }

    #[test]
    fn detection_accepts_declared_dependency_only_without_configuration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("requirements-dev.txt"), "black\npytest>=8\n").unwrap();
        assert_eq!(detect(dir.path()).unwrap().evidence, vec!["requirements-dev.txt"]);
        // Valid pip forms: a comment after whitespace, a direct reference, a continuation,
        // the old parenthesised form, any case.
        for line in [
            "pytest  # test runner\n",
            "pytest @ https://example.com/pytest.whl\n",
            "pytest@https://example.com/pytest.whl\n",
            "pytest \\\n    --hash=sha256:abc\n",
            "pytest (>=7)\n",
            "PyTest==8.0\n",
        ] {
            std::fs::write(dir.path().join("requirements-dev.txt"), line).unwrap();
            assert_eq!(
                detect(dir.path()).map(|r| r.evidence),
                Some(vec!["requirements-dev.txt".to_string()]),
                "{line:?}"
            );
        }
        for line in ["pytest-cov-but-not-really-pytestish\n", "pytest_cov\n", "# pytest\n", "notpytest\n"] {
            std::fs::write(dir.path().join("requirements-dev.txt"), line).unwrap();
            assert!(detect(dir.path()).is_none(), "{line:?}");
        }
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project.optional-dependencies]\ntest = [\"pytest\", \"hypothesis\"]\n",
        )
        .unwrap();
        assert_eq!(detect(dir.path()).unwrap().evidence, vec!["pyproject.toml"]);
        std::fs::write(dir.path().join("setup.cfg"), "[tool:pytest]\ntestpaths = tests\n").unwrap();
        assert_eq!(detect(dir.path()).unwrap().evidence, vec!["setup.cfg"], "configuration wins over declarations");
    }

    #[test]
    fn node_ids_are_exact_and_cannot_inject_options() {
        assert_eq!(node_id(FILE, ID_).unwrap(), ID_);
        assert!(node_id(FILE, "tests/test_issue_7.py::TestFrame::test_short").is_ok());
        for bad in [
            "test_issue_7_fails",
            "tests/test_issue_7.py",
            "tests/test_issue_7.py::test_x[1-2]",
            "tests/test_issue_7.py::helper",
            "tests/test_issue_7.py::Frame::test_short",
            "tests/test_issue_7.py::TestA::TestB::test_c",
            "tests/test_other.py::test_x",
            "tests/test_issue_7.py::test_x or test_y",
        ] {
            assert!(node_id(FILE, bad).is_err(), "{bad}");
        }
        for file in ["tests/conftest.py", "tests/helpers.py", "-p/test_x.py", "tests/test x.py", "tests/test_x.txt"] {
            assert!(validate_test_module(file).is_err(), "{file}");
        }
        assert!(validate_test_module("pkg/tests/frame_test.py").is_ok());
        let mut r = rec();
        r.registration_file = Some("src/lib.py".into());
        assert!(PythonPytest.validate_record(&r).is_err());
    }

    #[test]
    fn command_pins_flags_and_keeps_the_node_id_one_argument() {
        let spec = PythonPytest.regression_command(&rec(), Path::new("/w")).unwrap();
        assert_eq!(spec.program, PYTHON);
        assert_eq!(spec.args.last().unwrap(), ID_);
        for flag in ["no:cacheprovider", "addopts=", "--rootdir=.", "--confcutdir=.", "-rA"] {
            assert!(spec.args.iter().any(|a| a == flag), "{flag}");
        }
        assert_eq!(PythonPytest.regression_command_line(&rec()), spec.display());
    }

    #[test]
    fn classification_covers_every_outcome() {
        assert_eq!(classify_pytest(FAILED, ID_), RegressionOutcome::Failed);
        assert_eq!(classify_pytest(PASSED, ID_), RegressionOutcome::Passed);
        assert_eq!(classify_pytest(SKIPPED, ID_), RegressionOutcome::Skipped);
        assert_eq!(classify_pytest(XFAILED, ID_), RegressionOutcome::Skipped);
        assert_eq!(classify_pytest(NOT_FOUND, ID_), RegressionOutcome::NotDiscovered);
        assert_eq!(classify_pytest(COLLECTION_ERROR, ID_), RegressionOutcome::SetupFailed);
        assert!(
            matches!(classify_pytest(OTHER_FAILED, ID_), RegressionOutcome::Other(m) if m.contains("not among them"))
        );
        // Truncated before the summary, garbled summary, missing pytest.
        let truncated = &FAILED[..FAILED.find("=== short test summary").unwrap()];
        assert!(matches!(classify_pytest(truncated, ID_), RegressionOutcome::Other(_)));
        assert!(matches!(classify_pytest("== 1 exploded in 0.1s ==", ID_), RegressionOutcome::Other(_)));
        assert!(matches!(
            classify_pytest("/usr/bin/python3: No module named pytest\n", ID_),
            RegressionOutcome::Other(m) if m.contains("not installed")
        ));
        // CRLF output parses the same.
        assert_eq!(classify_pytest(&FAILED.replace('\n', "\r\n"), ID_), RegressionOutcome::Failed);
    }

    #[test]
    fn summary_parsing() {
        assert_eq!(parse_summary(FAILED).unwrap().failed, 1);
        let mixed = parse_summary("=== 1 failed, 2 passed, 1 skipped, 3 warnings, 2 deselected in 1.20s (0:00:01) ===")
            .unwrap();
        assert_eq!((mixed.failed, mixed.passed, mixed.skipped, mixed.deselected), (1, 2, 1, 2));
        assert_eq!(parse_summary("no tests ran in 0.01s"), Some(Counts::default()));
        assert_eq!(parse_summary("nothing here"), None);
    }

    #[test]
    fn confirm_failure_requires_exactly_one_failing_node_and_status_one() {
        assert!(PythonPytest.confirm_failure(&output(1, FAILED), &rec()).is_ok());
        assert!(PythonPytest.confirm_failure(&output(2, FAILED), &rec()).is_err(), "interrupted run");
        let two = FAILED.replace("1 failed in", "1 failed, 1 passed in");
        assert!(PythonPytest.confirm_failure(&output(1, &two), &rec()).is_err());
        assert!(PythonPytest.confirm_failure(&output(1, OTHER_FAILED), &rec()).is_err());
    }

    #[test]
    fn assert_passed_requires_the_node_and_nothing_else() {
        assert!(assert_test_passed(PASSED, ID_).is_ok());
        assert!(assert_test_passed(FAILED, ID_).is_err());
        assert!(assert_test_passed(&PASSED.replace("1 passed in", "1 passed, 1 xpassed in"), ID_).is_err());
        assert!(assert_test_passed(SKIPPED, ID_).is_err());
        assert!(assert_test_passed(&PASSED.replace("1 passed in", "2 passed in"), ID_).is_err());
    }

    #[test]
    fn source_binding_requires_an_empty_completed_collection() {
        assert!(assert_absent_from_collection("\nno tests collected in 0.00s\n", FILE, ID_).is_ok());
        let still = format!("{ID_}\n\n1 test collected in 0.00s\n");
        assert!(assert_absent_from_collection(&still, FILE, ID_).is_err());
        assert!(assert_absent_from_collection(
            "tests/test_issue_7.py::test_injected\n1 test collected in 0.01s",
            FILE,
            ID_
        )
        .is_err());
        assert!(assert_absent_from_collection("no tests collected, 1 error in 0.01s", FILE, ID_).is_err());
        assert!(assert_absent_from_collection("", FILE, ID_).is_err(), "no summary is not evidence");

        let spec = PythonPytest.regression_command(&rec(), Path::new("/w")).unwrap();
        let spec = PythonPytest.reproduction_environment(spec, Path::new("/run"));
        let discovery = PythonPytest.discovery(&spec);
        assert_eq!(discovery.spec.args[..2], ["-c".to_string(), DISCOVERY_WRAPPER.to_string()]);
        assert_eq!(discovery.spec.args.last().unwrap(), FILE, "the whole module, not the node");
        assert!(discovery.spec.args.iter().any(|a| a == "--collect-only"));
        assert!(
            discovery.spec.clear_env
                && discovery.spec.env.iter().any(|(k, v)| k == "PYTHONDONTWRITEBYTECODE" && v == "1")
        );
    }

    #[test]
    fn repro_changes_allowed_set() {
        let new = HashMap::from([("tests/__init__.py", ""), ("tests/pkg/__init__.py", "import os\n")]);
        let read = reader(&new);
        let ok = vec![
            ("??".into(), FILE.into()),
            ("??".into(), "tests/fixtures/frame.bin".into()),
            ("??".into(), "tests/__init__.py".into()),
        ];
        assert!(check_repro_changes_with(&ok, &rec(), &read).is_ok());
        for (status, path) in [
            ("M", FILE),
            ("??", "tests/conftest.py"),
            ("??", "conftest.py"),
            ("??", "tests/helpers.py"),
            ("??", "tests/pkg/__init__.py"),
            ("??", "pytest.ini"),
            ("M", "pyproject.toml"),
            ("M", "src/app.py"),
            ("??", "tests/sitecustomize.py"),
            ("??", "lib/evil.pth"),
        ] {
            let mut changes = vec![("??".to_string(), FILE.to_string())];
            if path == FILE {
                changes = vec![(status.to_string(), FILE.to_string())];
            } else {
                changes.push((status.into(), path.into()));
            }
            assert!(check_repro_changes_with(&changes, &rec(), &read).is_err(), "{status} {path}");
        }
        assert!(check_repro_changes_with(&[("??".into(), "tests/test_other.py".into())], &rec(), &read).is_err());
    }

    #[test]
    fn fix_lock_rejects_test_conftest_config_and_hook_changes() {
        let pyproject =
            "[project]\nname = \"x\"\n[tool.setuptools]\npy-modules = [\"app\"]\n[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n";
        let frozen = HashMap::from([
            (FILE, "def test_issue_7_fails():\n    assert 0\n"),
            ("pyproject.toml", pyproject),
            ("src/app.py", "x = 1\n"),
        ]);
        let fixed_source = HashMap::from([
            (FILE, "def test_issue_7_fails():\n    assert 0\n"),
            ("pyproject.toml", pyproject),
            ("src/app.py", "x = 2\n"),
        ]);
        let ok = vec![("M".to_string(), "src/app.py".to_string())];
        assert!(check_fix_changes_with(&ok, &rec(), &reader(&frozen), &reader(&fixed_source)).is_ok());
        assert!(check_fix_changes_with(&[], &rec(), &reader(&frozen), &reader(&fixed_source)).is_err());

        // A dependency change outside the pytest sections is allowed; a pytest section is not.
        let deps = pyproject.replace("name = \"x\"", "name = \"x\"\ndependencies = [\"attrs\"]");
        let now = HashMap::from([(FILE, frozen[FILE]), ("pyproject.toml", deps.as_str())]);
        assert!(check_fix_changes_with(
            &[("M".into(), "pyproject.toml".into())],
            &rec(),
            &reader(&frozen),
            &reader(&now)
        )
        .is_ok());
        let testpaths = pyproject.replace("[\"tests\"]", "[\"src\"]");
        let now = HashMap::from([(FILE, frozen[FILE]), ("pyproject.toml", testpaths.as_str())]);
        assert!(check_fix_changes_with(
            &[("M".into(), "pyproject.toml".into())],
            &rec(),
            &reader(&frozen),
            &reader(&now)
        )
        .is_err());
        let plugin = format!("{pyproject}[project.entry-points.pytest11]\nhide = \"x.plugin\"\n");
        let now = HashMap::from([(FILE, frozen[FILE]), ("pyproject.toml", plugin.as_str())]);
        assert!(check_fix_changes_with(
            &[("M".into(), "pyproject.toml".into())],
            &rec(),
            &reader(&frozen),
            &reader(&now)
        )
        .is_err());

        for path in ["conftest.py", "tests/conftest.py", "pytest.ini", "src/sitecustomize.py", "x.pth"] {
            let changes = vec![("M".to_string(), "src/app.py".to_string()), ("??".to_string(), path.to_string())];
            assert!(
                check_fix_changes_with(&changes, &rec(), &reader(&frozen), &reader(&fixed_source)).is_err(),
                "{path}"
            );
        }
        let edited = HashMap::from([(FILE, "def test_issue_7_fails():\n    assert 1\n")]);
        assert!(check_fix_changes_with(&ok, &rec(), &reader(&frozen), &reader(&edited)).is_err(), "frozen test edited");
        let mut changed_command = rec();
        changed_command.command = format!("{PYTHON} -m pytest {ID_}");
        assert!(check_fix_changes_with(&ok, &changed_command, &reader(&frozen), &reader(&fixed_source)).is_err());
    }

    #[test]
    fn fix_lock_freezes_test_package_initializers_helpers_and_data() {
        let frozen = HashMap::from([
            (FILE, "from tests.helpers import expected\n"),
            ("pyproject.toml", "[tool.setuptools]\npackages = [\"textkit\"]\n"),
        ]);
        let read = reader(&frozen);
        let check = |path: &str, status: &str| {
            let changes =
                vec![("M".to_string(), "textkit/lists.py".to_string()), (status.to_string(), path.to_string())];
            check_fix_changes_with(&changes, &rec(), &read, &read)
        };
        for (path, status) in [
            ("tests/__init__.py", "M"),
            ("tests/__init__.py", "??"),
            ("tests/helpers.py", "M"),
            ("tests/util/__init__.py", "D"),
            ("tests/textkit.py", "??"),
            ("tests/data/golden.txt", "M"),
        ] {
            let err = check(path, status).unwrap_err();
            assert!(err.contains("frozen with the regression"), "{path}: {err}");
        }
        assert!(check("tests/test_other.py", "M").is_err(), "every test module is frozen");
        assert!(check("textkit/__init__.py", "M").is_ok(), "production package initializers may change");
        assert_eq!(test_tree("src/pkg/tests/unit/test_x.py"), "src/pkg/tests");
        assert_eq!(test_tree("pkg/test_x.py"), "", "a test next to the code does not make the package test code");
        assert_eq!(test_tree("test_x.py"), "");
    }

    /// The lock is an allowlist: a fix may change declared production code and the dependency
    /// fields of the root manifests, nothing else. Frozen tests, helpers and fixtures take
    /// precedence even under `src/` or a declared package; a manifest-named fixture is a
    /// fixture. Additions, deletions and renames (a deletion plus an addition) are all checked.
    #[test]
    fn fix_may_change_only_declared_production_code() {
        let pyproject = "[project]\nname = \"calc\"\ndependencies = [\"attrs\"]\n[tool.setuptools]\npackages = [\"calc\", \"textkit\", \"app\"]\n[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n[project.entry-points.pytest11]\n";
        let setup_cfg = "[metadata]\nname = calc\n[options]\npackages = calc\ninstall_requires =\n    attrs>=21\n    six==1.16\n[options.extras_require]\ntest = pytest\n[tool:pytest]\ntestpaths = tests\n";
        let checkpoint = HashMap::from([
            (FILE, "from calc import add\n"),
            ("pyproject.toml", pyproject),
            ("setup.cfg", setup_cfg),
            ("requirements.txt", "attrs\n"),
            ("calc/__init__.py", ""),
            ("calc/core.py", "x = 1\n"),
            ("src/app/__init__.py", ""),
            ("src/app/a.py", "x = 1\n"),
            ("src/app/data.json", "{}\n"),
            ("src/app/tests/fixtures/input.json", "{}\n"),
            ("calc/fixtures/table.csv", "1\n"),
            ("fixtures/pyproject.toml", "[project]\nname = \"fixture\"\n"),
            ("fixtures/input.json", "{}\n"),
            ("README.md", "readme\n"),
        ]);
        let read_checkpoint = reader(&checkpoint);
        let check_with = |changes: &[(&str, &str)], now: &HashMap<&str, &str>, record: &ReproRecord| {
            let mut all = vec![("M".to_string(), "calc/core.py".to_string())];
            all.extend(changes.iter().map(|(s, p)| (s.to_string(), p.to_string())));
            check_fix_changes_with(&all, record, &read_checkpoint, &reader(now))
        };
        let check = |changes: &[(&str, &str)]| check_with(changes, &checkpoint, &rec());

        // Production code: modify, add inside a package, delete, rename.
        for changes in [
            vec![("M", "src/app/a.py")],
            vec![("A", "src/app/new.py")],
            vec![("D", "src/app/a.py")],
            vec![("D", "src/app/a.py"), ("A", "src/app/b.py")],
            vec![("A", "calc/util.py")],
        ] {
            assert!(check(&changes).is_ok(), "{changes:?}");
        }
        // Package data inside production code may be a test input: frozen, under src/ and in
        // a declared package, without a `tests` or `fixtures` directory in its path.
        for path in ["src/app/data.json", "src/app/data/test_vector.json", "calc/table.csv"] {
            let err = check(&[("M", path)]).unwrap_err();
            assert!(err.contains(path) && err.contains("only source code may change"), "{path}: {err}");
        }
        // Everything else, in every form.
        for changes in [
            vec![("M", "README.md")],
            vec![("A", "docs/guide.md")],
            vec![("M", "tests/test_other.py")],
            vec![("A", "tests/test_new.py")],
            vec![("M", "src/app/tests/fixtures/input.json")],
            vec![("D", "src/app/tests/fixtures/input.json")],
            vec![("D", "src/app/tests/fixtures/input.json"), ("A", "src/app/tests/fixtures/moved.json")],
            vec![("A", "src/app/tests/fixtures/extra.json")],
            vec![("M", "calc/fixtures/table.csv")],
            vec![("M", "fixtures/pyproject.toml")],
            vec![("D", "fixtures/pyproject.toml")],
            vec![("D", "fixtures/pyproject.toml"), ("A", "fixtures/renamed.toml")],
            vec![("A", "fixtures/setup.cfg")],
            vec![("A", "fixtures/requirements.txt")],
            vec![("M", "fixtures/input.json")],
            vec![("M", "support/helpers.py")],
            vec![("M", "tox.ini")],
            vec![("M", "lyra.toml")],
            vec![("A", "src/pytest.py")],
            vec![("A", "src/_pytest/__init__.py")],
            vec![("A", "src/json.cpython-314-darwin.so")],
            vec![("A", "requirements-extra.txt")],
            vec![("D", "requirements.txt")],
            vec![("D", "pyproject.toml")],
        ] {
            let err = check(&changes).expect_err(&format!("{changes:?}"));
            let last = changes.last().unwrap().1;
            assert!(err.contains(last), "{changes:?}: {err}");
        }
        // A frozen input takes precedence over the production allowlist, even under src/.
        let mut record = rec();
        record.frozen_inputs = vec!["src/app/data.json".into()];
        let mut edited = checkpoint.clone();
        edited.insert("src/app/data.json", "{\"benign\": true}\n");
        let err = check_with(&[("M", "src/app/data.json")], &edited, &record).unwrap_err();
        assert!(err.contains("src/app/data.json") && err.contains("locked"), "{err}");

        // Root manifests: dependency fields only.
        let manifest = |path: &'static str, text: String| {
            let mut now = checkpoint.clone();
            now.insert(path, Box::leak(text.into_boxed_str()));
            check_with(&[("M", path)], &now, &rec())
        };
        assert!(manifest("pyproject.toml", pyproject.replace("[\"attrs\"]", "[\"attrs>=23\", \"six\"]")).is_ok());
        assert!(manifest(
            "pyproject.toml",
            format!("{pyproject}[project.optional-dependencies]\nfast = [\"ujson\"]\n[dependency-groups]\ndev = [\"ruff\"]\n")
        )
        .is_ok());
        for broken in [
            pyproject.replace("name = \"calc\"", "name = \"other\""),
            pyproject.replace("testpaths = [\"tests\"]", "testpaths = [\"src\"]"),
            pyproject.replace("[\"calc\", \"textkit\", \"app\"]", "[\"calc\"]"),
            format!("{pyproject}hide = \"calc.plugin\"\n"),
            "[project\n".to_string(),
        ] {
            assert!(manifest("pyproject.toml", broken.clone()).is_err(), "{broken}");
        }
        assert!(manifest("setup.cfg", setup_cfg.replace("    six==1.16\n", "    six==1.17\n    idna>=3\n")).is_ok());
        assert!(manifest("setup.cfg", setup_cfg.replace("test = pytest", "test = pytest\nfast = ujson")).is_ok());
        for broken in [
            setup_cfg.replace("packages = calc", "packages = other"),
            setup_cfg.replace("testpaths = tests", "testpaths = src"),
        ] {
            assert!(manifest("setup.cfg", broken.clone()).is_err(), "{broken}");
        }
        assert!(manifest("requirements.txt", "attrs>=23\nsix\n".into()).is_ok());

        // A frozen pyproject.toml that cannot be parsed cannot be compared field by field: it
        // stays frozen, whatever the replacement (the reviewer's malformed-original case).
        let mut broken = checkpoint.clone();
        broken.insert("pyproject.toml", "[project\nname = \"calc\"\n[tool.setuptools]\npackages = [\"calc\"]\n");
        let read_broken = reader(&broken);
        for replacement in [
            "[project]\nname = \"calc\"\ndependencies = [\"attrs\"]\n".to_string(),
            "[tool.pytest.ini_options]\naddopts = \"-k not issue\"\n".to_string(),
        ] {
            let mut now = broken.clone();
            now.insert("pyproject.toml", Box::leak(replacement.into_boxed_str()));
            let changes = vec![("M".to_string(), "pyproject.toml".to_string())];
            let err = check_fix_changes_with(&changes, &rec(), &read_broken, &reader(&now)).unwrap_err();
            assert!(err.contains("not valid TOML") && err.contains("stays frozen"), "{err}");
        }
    }

    #[test]
    fn verification_steps_default_to_the_suite_and_skip_unconfigured_optional_steps() {
        let steps = PythonPytest.verification_steps(&VerifyCommands::default(), &rec(), Path::new("/w")).unwrap();
        let names: Vec<&str> = steps.iter().map(|s| s.name).collect();
        assert_eq!(names, ["repro", "tests"]);
        assert_eq!(steps[1].spec.display(), format!("{PYTHON} -m pytest"));
        let configured = VerifyCommands {
            build: Some("python3 -m compileall -q src".into()),
            test: Some("python3 -m pytest tests".into()),
            fmt: Some("ruff format --check .".into()),
        };
        let names: Vec<&str> = PythonPytest
            .verification_steps(&configured, &rec(), Path::new("/w"))
            .unwrap()
            .iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["build", "repro", "tests", "fmt"]);
        let empty_suite = VerifyCommands { test: Some(" ".into()), ..Default::default() };
        assert!(PythonPytest.verification_steps(&empty_suite, &rec(), Path::new("/w")).is_err());
    }

    #[test]
    fn environment_scrubs_operator_python_settings_and_redirects_caches() {
        std::env::set_var("PYTEST_ADDOPTS", "-k nothing");
        std::env::set_var("PYTHONPATH", "/somewhere");
        let spec = PythonPytest.command_environment(CommandSpec::new(PYTHON, "/w"), Path::new("/env"));
        std::env::remove_var("PYTEST_ADDOPTS");
        std::env::remove_var("PYTHONPATH");
        assert!(spec.clear_env);
        assert!(
            !spec.env.iter().any(|(k, v)| k == "PYTHONPATH" && v.contains("/somewhere")),
            "operator PYTHONPATH is dropped"
        );
        let addopts: Vec<&str> =
            spec.env.iter().filter(|(k, _)| k == "PYTEST_ADDOPTS").map(|(_, v)| v.as_str()).collect();
        assert_eq!(addopts, ["-p no:cacheprovider"]);
        assert!(spec.env.iter().any(|(k, v)| k == "RUFF_CACHE_DIR" && v.starts_with("/env")));
    }

    #[test]
    fn ancestor_pytest_configuration_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("a/b/checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        assert!(ensure_no_ancestor_pytest_config(&checkout).is_ok());
        std::fs::write(dir.path().join("a/conftest.py"), "").unwrap();
        assert!(ensure_no_ancestor_pytest_config(&checkout).unwrap_err().contains("conftest.py"));
    }

    #[test]
    fn layout_description_reads_testpaths_without_executing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pytest.ini"), "[pytest]\ntestpaths = tests integration\n").unwrap();
        std::fs::create_dir(dir.path().join("tests")).unwrap();
        let v = describe_layout(dir.path());
        assert_eq!(v["testpaths"], json!(["tests", "integration"]));
        assert_eq!(v["test_dirs"], json!(["tests"]));
        assert_eq!(v["config_files"], json!(["pytest.ini"]));
        let toml = tempfile::tempdir().unwrap();
        std::fs::write(
            toml.path().join("pyproject.toml"),
            "[tool.pytest.ini_options]\ntestpaths = [\"tests\", \"more\"]\n",
        )
        .unwrap();
        assert_eq!(describe_layout(toml.path())["testpaths"], json!(["tests", "more"]));
    }

    /// Runs the real commands against a scratch project. Needs `python3` (Windows: `python`)
    /// with pytest on PATH: `cargo test python_pytest -- --ignored`.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn real_pytest_failure_binding_and_pass_after_fix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("pyproject.toml"), "[tool.pytest.ini_options]\naddopts = \"-x -q --maxfail=1\"\n")
            .unwrap();
        std::fs::write(root.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
        std::fs::create_dir(root.join("tests")).unwrap();
        std::fs::write(root.join("tests/test_existing.py"), "def test_unrelated():\n    assert True\n").unwrap();
        std::fs::write(
            root.join("tests/test_issue_7.py"),
            "from calc import add\n\ndef test_issue_7_fails():\n    assert add(1, 2) == 3, \"sum must be three\"\n",
        )
        .unwrap();
        assert_eq!(detect(root).unwrap().evidence, vec!["pyproject.toml"]);
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let profile = PythonPytest;
        let record = rec();

        let spec = profile.reproduction_environment(profile.regression_command(&record, root).unwrap(), root);
        let run = proc::run(&spec, timeout, &cancel).await.unwrap();
        let out = run.combined();
        assert_eq!(profile.classify(&out, &record), RegressionOutcome::Failed, "{out}");
        profile.confirm_failure(&run, &record).unwrap();
        assert!(guards::output_contains_expected(&out, &record.expected));

        let original = std::fs::read(root.join(FILE)).unwrap();
        std::fs::write(root.join(FILE), "").unwrap();
        let discovery = profile.discovery(&spec);
        let bound = proc::run(&discovery.spec, timeout, &cancel).await.unwrap();
        std::fs::write(root.join(FILE), &original).unwrap();
        assert!(bound.success(), "{}", bound.combined());
        profile.assert_absent_from_discovery(&bound.stdout, &record).unwrap();

        std::fs::write(root.join("calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
        for step in profile.verification_steps(&VerifyCommands::default(), &record, root).unwrap() {
            let run =
                proc::run(&profile.command_environment(step.spec, &root.join("env")), timeout, &cancel).await.unwrap();
            assert!(run.success(), "{}: {}", step.name, run.combined());
            if step.regression {
                profile.assert_passed(&run.combined(), &record).unwrap();
            }
        }
        assert!(
            !root.join(".pytest_cache").exists() && !root.join("tests/__pycache__").exists(),
            "checkout stays clean"
        );
    }

    /// An editable install of another clone must not stand in for the candidate checkout.
    /// A venv whose site-packages has a `.pth` entry for clone A's `src/` (what an editable
    /// install leaves behind) runs checkout B; only B's code may decide the results, and the
    /// provenance step must fail when imports are not bound to B.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn imports_come_from_the_checkout_not_an_editable_install_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let write = |path: std::path::PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        // Clone A (the operator's own checkout, installed editable): correct behaviour.
        let clone_a = dir.path().join("clone-a");
        write(clone_a.join("src/pkg/__init__.py"), "def answer():\n    return 42\n");
        // Checkout B (the candidate under test): the bug.
        let checkout = dir.path().join("checkout");
        write(checkout.join("src/pkg/__init__.py"), "def answer():\n    return 41\n");
        write(checkout.join("pytest.ini"), "[pytest]\n");
        write(
            checkout.join(FILE),
            "from pkg import answer\n\ndef test_issue_7_fails():\n    assert answer() == 42, \"sum must be three\"\n",
        );

        // A venv without pip whose site-packages points at clone A, like `pip install -e`.
        let venv = dir.path().join("venv");
        let made = proc::run(
            &CommandSpec::new(PYTHON, dir.path()).args(["-m", "venv", "--without-pip"]).arg(venv.to_string_lossy()),
            timeout,
            &cancel,
        )
        .await
        .unwrap();
        assert!(made.success(), "{}", made.combined());
        let venv_python = if cfg!(windows) { venv.join("Scripts/python.exe") } else { venv.join("bin/python") };
        let purelib = proc::run(
            &CommandSpec::new(venv_python.to_string_lossy(), dir.path())
                .args(["-c", "import sysconfig; print(sysconfig.get_paths()['purelib'])"]),
            timeout,
            &cancel,
        )
        .await
        .unwrap();
        write(
            std::path::PathBuf::from(purelib.stdout.trim()).join("__editable__pkg.pth"),
            &format!("{}\n", clone_a.join("src").display()),
        );
        // pytest itself comes from the provisioned interpreter, as a system-site package would.
        let pytest_site = proc::run(
            &CommandSpec::new(PYTHON, dir.path())
                .args(["-c", "import os, pytest; print(os.path.dirname(os.path.dirname(pytest.__file__)))"]),
            timeout,
            &cancel,
        )
        .await
        .unwrap();
        write(
            std::path::PathBuf::from(purelib.stdout.trim()).join("provisioned.pth"),
            &format!("{}\n", pytest_site.stdout.trim()),
        );
        let with_venv = |mut spec: CommandSpec| {
            spec.program = venv_python.to_string_lossy().into_owned();
            spec
        };

        // Unbound (the old behaviour): `import pkg` resolves to clone A and the bug is hidden.
        let mut unbound = with_venv(PythonPytest.regression_command(&rec(), &checkout).unwrap()).scrubbed();
        unbound.env.push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
        let out = proc::run(&unbound, timeout, &cancel).await.unwrap().combined();
        assert_eq!(
            classify_pytest(&out, ID_),
            RegressionOutcome::Passed,
            "precondition: the editable install leaks: {out}"
        );

        // Bound: the candidate's code decides, in repro and in verification.
        let spec = with_venv(
            PythonPytest
                .reproduction_environment(PythonPytest.regression_command(&rec(), &checkout).unwrap(), dir.path()),
        );
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");

        let steps = PythonPytest.verification_steps(&VerifyCommands::default(), &rec(), &checkout).unwrap();
        assert_eq!(steps[0].name, "imports");
        assert_eq!(steps[0].spec.stdin.as_deref(), Some("pkg\n"));
        let provenance = steps[0].spec.clone();
        let bound = proc::run(
            &with_venv(PythonPytest.command_environment(provenance.clone(), &dir.path().join("env"))),
            timeout,
            &cancel,
        )
        .await
        .unwrap();
        assert!(bound.success() && bound.stdout.contains("pkg -> "), "{}", bound.combined());
        // Without the binding the provenance step fails visibly and names the package.
        let mut leaked = with_venv(provenance).scrubbed();
        leaked.env.push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
        let leaked = proc::run(&leaked, timeout, &cancel).await.unwrap();
        assert!(
            !leaked.success() && leaked.stdout.contains("ERROR: pkg does not resolve inside the checkout"),
            "{}",
            leaked.combined()
        );
        assert!(PythonPytest.explain_failure(&leaked.stdout, leaked.exit_code).contains("pkg"));

        // The candidate's fix (and only it) makes the bound regression pass.
        write(checkout.join("src/pkg/__init__.py"), "def answer():\n    return 42\n");
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Passed, "{out}");
    }

    /// Changing a helper the frozen test imports would make the unchanged failing test pass;
    /// the lock rejects it.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn changing_a_test_helper_could_pass_the_frozen_test_and_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("pytest.ini"), "[pytest]\n").unwrap();
        std::fs::write(root.join("pyproject.toml"), PYPROJECT).unwrap();
        std::fs::write(root.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
        std::fs::write(root.join("tests/__init__.py"), "").unwrap();
        std::fs::write(root.join("tests/helpers.py"), "EXPECTED = 3\n").unwrap();
        std::fs::write(root.join("tests/test_helpers.py"), "LIMIT = 0\n").unwrap();
        let test = "from calc import add\nfrom tests.helpers import EXPECTED\nfrom tests.test_helpers import LIMIT\n\ndef test_issue_7_fails():\n    assert add(1, 2) == EXPECTED + LIMIT, \"sum must be three\"\n";
        std::fs::write(root.join(FILE), test).unwrap();
        let spec = PythonPytest.reproduction_environment(PythonPytest.regression_command(&rec(), root).unwrap(), root);
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");
        let frozen: HashMap<&str, String> = [FILE, "tests/helpers.py", "tests/__init__.py", "tests/test_helpers.py"]
            .into_iter()
            .map(|p| (p, std::fs::read_to_string(root.join(p)).unwrap()))
            .collect();

        for (path, text) in [
            ("tests/helpers.py", "EXPECTED = -1\n"),
            ("tests/__init__.py", "import calc\ncalc.add = lambda a, b: 3\n"),
            ("tests/test_helpers.py", "LIMIT = -4\n"),
        ] {
            let original = std::fs::read_to_string(root.join(path)).unwrap();
            std::fs::write(root.join(path), text).unwrap();
            let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
            assert_eq!(
                classify_pytest(&out, ID_),
                RegressionOutcome::Passed,
                "the attack works without the lock: {out}"
            );
            let read_checkpoint = |p: &str| frozen.get(p).cloned();
            let read_now = |p: &str| std::fs::read_to_string(root.join(p)).ok();
            let err =
                check_fix_changes_with(&[("M".into(), path.into())], &rec(), &read_checkpoint, &read_now).unwrap_err();
            assert!(err.contains(path) && err.contains("test lock"), "{err}");
            std::fs::write(root.join(path), original).unwrap();
        }
    }

    #[test]
    fn requirements_only_projects_put_their_dependency_files_in_the_source_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("requirements.txt"), "attrs==23.1\n").unwrap();
        std::fs::write(root.join("requirements-dev.txt"), "pytest>=8\n").unwrap();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(root.join("app/core.py"), "import attrs\n").unwrap();
        assert_eq!(detect(root).unwrap().evidence, vec!["requirements-dev.txt"]);
        // In the overview without being named...
        let overview = crate::stages::readonly_source_snapshot(root, &[], &PythonPytest);
        assert!(overview.contains("File: requirements.txt") && overview.contains("attrs==23.1"), "{overview}");
        assert!(overview.contains("File: requirements-dev.txt"));
        // ...and completely, as an ancestor manifest of a relevant file.
        let focused = crate::stages::readonly_source_snapshot(root, &["app/core.py".to_string()], &PythonPytest);
        let complete = &focused[..focused.find("[Fallback source overview").unwrap()];
        assert!(
            complete.contains("File: requirements.txt") && complete.contains("File: requirements-dev.txt"),
            "{complete}"
        );
    }

    #[test]
    fn packages_named_env_or_build_stay_visible_while_real_environments_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("pytest.ini", "[pytest]\n");
        write("src/env/settings.py", "DEBUG = False\n");
        write("src/build/parser.py", "def parse(): pass\n");
        write("env/__init__.py", "");
        write("env/config.py", "ENV = 1\n");
        write("venv/pyvenv.cfg", "home = /usr/bin\n");
        write("venv/lib/python3.12/site-packages/dep/__init__.py", "LEAKED_DEPENDENCY = 1\n");
        write("myenv/pyvenv.cfg", "home = /usr/bin\n");
        write("myenv/lib/x.py", "LEAKED_RENAMED_VENV = 1\n");
        write("build/lib/pkg/__init__.py", "LEAKED_BUILD_OUTPUT = 1\n");
        write("dist/pkg-0.1.tar.gz", "");
        let snapshot = crate::stages::readonly_source_snapshot(root, &[], &PythonPytest);
        for visible in ["src/env/settings.py", "src/build/parser.py", "env/config.py"] {
            assert!(snapshot.contains(&format!("File: {visible}")), "{visible} missing:\n{snapshot}");
        }
        for hidden in ["LEAKED_DEPENDENCY", "LEAKED_RENAMED_VENV", "LEAKED_BUILD_OUTPUT"] {
            assert!(!snapshot.contains(hidden), "{hidden} leaked:\n{snapshot}");
        }
        assert!(!is_generated_python_dir(Path::new("dist"), &root.join("src")), "a non-archive dist is source");
    }

    #[test]
    fn provenance_names_cover_project_packages_and_modules_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (rel, text) in [
            ("src/pkg/__init__.py", ""),
            ("toplevel/__init__.py", ""),
            ("calc.py", ""),
            ("setup.py", ""),
            ("conftest.py", ""),
            ("test_root.py", ""),
            ("tests/__init__.py", ""),
            ("docs/__init__.py", ""),
            ("venv/pyvenv.cfg", ""),
            ("venv/__init__.py", ""),
            ("not-a-name/__init__.py", ""),
            ("namespace_only/mod.py", ""),
            // A namespace root that holds only subpackages: no `.py` directly in `src/acme/`.
            ("src/acme/widget/mod.py", ""),
            ("src/empty_dirs/nothing/.keep", ""),
            // A source namespace package named like build output; real build output at the root
            // (`build/lib/...`) is still skipped by its markers.
            ("src/build/parser.py", ""),
            ("build/lib/generated/__init__.py", ""),
            // Imported by the regression from `lib/`, which pytest's `pythonpath` puts on the path.
            ("pytest.ini", "[pytest]\npythonpath = lib\n"),
            ("lib/vendor/deep/mod.py", ""),
            // Names that are not project code at a search path's top: a stdlib import next to a
            // nested `json.py`, a quoted word next to a nested `status.py`, a root `__main__.py`.
            ("src/pkg/serializers/json.py", ""),
            ("src/pkg/status.py", ""),
            ("__main__.py", ""),
            (FILE, "import json\nfrom acme.widget import mod\nimport vendor.deep.mod\nimport os\nSTATE = \"status\"\n"),
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        assert_eq!(importable_names(root), ["acme", "build", "calc", "pkg", "toplevel"]);
        assert_eq!(provenance_names(root, FILE), ["acme", "build", "calc", "pkg", "toplevel", "vendor"]);
        let steps = PythonPytest.verification_steps(&VerifyCommands::default(), &rec(), root).unwrap();
        assert_eq!(steps.iter().map(|s| s.name).collect::<Vec<_>>(), ["imports", "repro", "tests"]);
        assert_eq!(steps[0].spec.args.len(), 2, "names travel on stdin, not argv");
        assert_eq!(steps[0].spec.stdin.as_deref(), Some("acme\nbuild\ncalc\npkg\ntoplevel\nvendor\n"));
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            PythonPytest.verification_steps(&VerifyCommands::default(), &rec(), empty.path()).unwrap()[0].name,
            "repro"
        );
    }

    #[test]
    fn environment_binds_imports_to_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let spec = PythonPytest.command_environment(CommandSpec::new(PYTHON, dir.path()), Path::new("/env"));
        let python_path: Vec<&str> =
            spec.env.iter().filter(|(k, _)| k == "PYTHONPATH").map(|(_, v)| v.as_str()).collect();
        assert_eq!(python_path.len(), 1);
        let entries: Vec<std::path::PathBuf> = std::env::split_paths(python_path[0]).collect();
        assert_eq!(entries, [dir.path().to_path_buf(), dir.path().join("src")]);
        let flat = tempfile::tempdir().unwrap();
        let spec = PythonPytest.reproduction_environment(CommandSpec::new(PYTHON, flat.path()), Path::new("/run"));
        let value = &spec.env.iter().find(|(k, _)| k == "PYTHONPATH").unwrap().1;
        assert_eq!(
            std::env::split_paths(value).collect::<Vec<_>>(),
            [flat.path().to_path_buf()],
            "no src/ entry when absent"
        );
    }

    #[test]
    fn import_parsing_covers_the_forms_a_regression_uses() {
        let source = "import os, sys as system\nfrom tests.test_helpers import (\n    EXPECTED,\n    build,  # comment\n)\nfrom . import test_shared\nfrom ..pkg.mod import thing\nfrom calc import add\nimport a.b \\\n    as ab\ndef f():\n    import late_helper\n    mod = importlib.import_module('tests.dynamic_helper')\nfrom x import *\n";
        let modules = imported_modules(source);
        for expected in [
            "os",
            "sys",
            "tests.test_helpers",
            "tests.test_helpers.EXPECTED",
            "tests.test_helpers.build",
            ".test_shared",
            "..pkg.mod",
            "..pkg.mod.thing",
            "calc",
            "calc.add",
            "a.b",
            "late_helper",
            "tests.dynamic_helper",
            "x",
        ] {
            assert!(modules.contains(&expected.to_string()), "{expected} missing from {modules:?}");
        }
        assert!(!modules.iter().any(|m| m.ends_with('*') || m == "."));
    }

    #[test]
    fn modules_imported_by_the_regression_are_locked_even_when_named_like_tests() {
        let frozen_test = "from tests.test_helpers import EXPECTED\nfrom .test_shared import build\nfrom calc import add\n\ndef test_issue_7_fails():\n    assert add(1, 2) == EXPECTED, \"sum must be three\"\n";
        let files = HashMap::from([
            (FILE, frozen_test),
            ("tests/__init__.py", ""),
            ("tests/test_helpers.py", "from tests.test_base import BASE\nEXPECTED = BASE + 3\n"),
            ("tests/test_base.py", "BASE = 0\n"),
            ("tests/test_shared.py", "def build(): pass\n"),
            ("tests/test_other.py", "def test_other(): pass\n"),
            ("calc.py", "def add(a, b): return a - b\n"),
            ("pyproject.toml", PYPROJECT),
        ]);
        let read = reader(&files);
        let locked = regression_imports(FILE, &read, &declared()).unwrap();
        assert_eq!(
            locked.iter().map(String::as_str).collect::<Vec<_>>(),
            ["tests/__init__.py", "tests/test_base.py", "tests/test_helpers.py", "tests/test_shared.py"],
            "direct, relative and transitive test-side imports; production `calc` is not followed"
        );
        let check = |path: &str| {
            let changes = vec![("M".to_string(), "calc.py".to_string()), ("M".to_string(), path.to_string())];
            check_fix_changes_with(&changes, &rec(), &read, &read)
        };
        for path in ["tests/test_helpers.py", "tests/test_base.py", "tests/test_shared.py"] {
            let err = check(path).unwrap_err();
            assert!(err.contains("frozen with the regression"), "{path}: {err}");
        }
        assert!(check("tests/test_other.py").is_err(), "every test module is frozen, imported or not");

        // pytest's default import mode puts the first directory without __init__.py on sys.path,
        // so a bare `import test_helpers` from tests/ resolves to tests/test_helpers.py.
        let bare =
            HashMap::from([(FILE, "from test_helpers import EXPECTED\n"), ("tests/test_helpers.py", "EXPECTED = 3\n")]);
        assert!(regression_imports(FILE, &reader(&bare), &declared()).unwrap().contains("tests/test_helpers.py"));
        // A helper named like a test outside the test tree is locked too.
        let outside = HashMap::from([
            (FILE, "from support.test_fixtures import DATA\n"),
            ("support/test_fixtures.py", "DATA = 1\n"),
        ]);
        assert!(regression_imports(FILE, &reader(&outside), &declared()).unwrap().contains("support/test_fixtures.py"));
    }

    #[test]
    fn provenance_covers_namespace_packages_imported_project_code_and_has_no_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("src/nspkg/module.py", "");
        write("lib/libpkg/__init__.py", "");
        // `lib/` is a search path because the repository declares packages there.
        write("pyproject.toml", "[tool.setuptools.packages.find]\nwhere = [\"lib\"]\ninclude = [\"libpkg\"]\n");
        write("tools/release.py", "");
        write("tests/test_helpers.py", "");
        write(
            FILE,
            "import os\nimport attrs\nfrom nspkg.module import x\nfrom libpkg import y\nfrom test_helpers import z\n",
        );
        let names = provenance_names(root, FILE);
        assert!(names.contains(&"nspkg".to_string()), "PEP 420 namespace package under src/: {names:?}");
        assert!(
            names.contains(&"libpkg".to_string()),
            "project code the regression imports from a declared search path: {names:?}"
        );
        for not_project in ["os", "attrs", "tools", "test_helpers"] {
            assert!(!names.contains(&not_project.to_string()), "{not_project}: {names:?}");
        }
        for i in 0..120 {
            write(&format!("src/mod_{i}.py"), "");
        }
        let all = provenance_names(root, FILE);
        assert!(all.len() > 120, "no truncation: {}", all.len());
        let steps = PythonPytest.verification_steps(&VerifyCommands::default(), &rec(), root).unwrap();
        assert_eq!(steps[0].name, "imports");
        assert_eq!(steps[0].spec.stdin.as_deref().unwrap().lines().count(), all.len());
    }

    /// A regular package of the same name outside the checkout takes precedence over the
    /// candidate's PEP 420 namespace package; the provenance step must catch it by name. Both
    /// a namespace package of modules (`src/pkg/module.py`) and a namespace root that holds
    /// only subpackages (`src/pkg/widget/module.py`, nothing directly in `src/pkg/`).
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn a_regular_package_elsewhere_cannot_hide_behind_a_namespace_package() {
        for (module, import) in [
            ("pkg/module.py", "from pkg.module import answer"),
            ("pkg/widget/module.py", "from pkg.widget.module import answer"),
            // A source namespace package named like generated output, imported under a computed
            // name, so the static import scan cannot see it: `src/build/` is still listed.
            ("build/parser.py", "import importlib\nanswer = importlib.import_module(\"bu\" + \"ild.parser\").answer"),
        ] {
            namespace_package_shadowed_externally(module, import).await;
        }
    }

    async fn namespace_package_shadowed_externally(module: &str, import: &str) {
        let dir = tempfile::tempdir().unwrap();
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let write = |path: std::path::PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        let checkout = dir.path().join("checkout");
        write(checkout.join("src").join(module), "def answer():\n    return 41\n");
        write(checkout.join("pytest.ini"), "[pytest]\n");
        write(
            checkout.join(FILE),
            &format!("{import}\n\ndef test_issue_7_fails():\n    assert answer() == 42, \"sum must be three\"\n"),
        );
        let package = module.split('/').next().unwrap();
        let external = dir.path().join("site");
        for dir in std::path::Path::new(module).ancestors().skip(1).filter(|d| !d.as_os_str().is_empty()) {
            write(external.join(dir).join("__init__.py"), "");
        }
        write(external.join(module), "def answer():\n    return 42\n");

        let venv = dir.path().join("venv");
        let made = proc::run(
            &CommandSpec::new(PYTHON, dir.path()).args(["-m", "venv", "--without-pip"]).arg(venv.to_string_lossy()),
            timeout,
            &cancel,
        )
        .await
        .unwrap();
        assert!(made.success(), "{}", made.combined());
        let venv_python = if cfg!(windows) { venv.join("Scripts/python.exe") } else { venv.join("bin/python") };
        let purelib = proc::run(
            &CommandSpec::new(venv_python.to_string_lossy(), dir.path())
                .args(["-c", "import sysconfig; print(sysconfig.get_paths()['purelib'])"]),
            timeout,
            &cancel,
        )
        .await
        .unwrap();
        let purelib = std::path::PathBuf::from(purelib.stdout.trim());
        write(purelib.join("external.pth"), &format!("{}\n", external.display()));
        let pytest_site = proc::run(
            &CommandSpec::new(PYTHON, dir.path())
                .args(["-c", "import os, pytest; print(os.path.dirname(os.path.dirname(pytest.__file__)))"]),
            timeout,
            &cancel,
        )
        .await
        .unwrap();
        write(purelib.join("provisioned.pth"), &format!("{}\n", pytest_site.stdout.trim()));
        let with_venv = |mut spec: CommandSpec| {
            spec.program = venv_python.to_string_lossy().into_owned();
            spec
        };

        // Even with imports bound to the checkout, Python prefers the external regular package
        // (PEP 420), so the bug is hidden: binding alone cannot fix this layout...
        let spec = with_venv(
            PythonPytest
                .reproduction_environment(PythonPytest.regression_command(&rec(), &checkout).unwrap(), dir.path()),
        );
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(
            classify_pytest(&out, ID_),
            RegressionOutcome::Passed,
            "precondition: the external package wins: {out}"
        );
        // ...so verification fails visibly, naming the package (which must be listed even
        // when `src/pkg/` itself holds no Python file).
        let steps = PythonPytest.verification_steps(&VerifyCommands::default(), &rec(), &checkout).unwrap();
        assert_eq!(steps[0].name, "imports", "{module}");
        assert_eq!(steps[0].spec.stdin.as_deref(), Some(format!("{package}\n").as_str()), "{module}");
        let env_root = dir.path().join("env");
        let check =
            proc::run(&with_venv(PythonPytest.command_environment(steps[0].spec.clone(), &env_root)), timeout, &cancel)
                .await
                .unwrap();
        assert!(
            !check.success()
                && check.stdout.contains(&format!("ERROR: {package} does not resolve inside the checkout")),
            "{}",
            check.combined()
        );

        // Once nothing shadows it, the namespace package resolves inside and the check passes.
        std::fs::remove_file(purelib.join("external.pth")).unwrap();
        let check =
            proc::run(&with_venv(PythonPytest.command_environment(steps[0].spec.clone(), &env_root)), timeout, &cancel)
                .await
                .unwrap();
        assert!(check.success(), "{}", check.combined());
    }

    #[test]
    fn inline_and_conditional_imports_are_found_and_the_limit_fails_visibly() {
        let source = "if True: from tests.test_helpers import EXPECTED\nsetup(); import tests.test_other\ntry: import tests.test_maybe\nexcept ImportError: pass\nwith ctx(): from .test_shared import build\nx = 1  # import tests.test_comment\n";
        let modules = imported_modules(source);
        for expected in [
            "tests.test_helpers",
            "tests.test_helpers.EXPECTED",
            "tests.test_other",
            "tests.test_maybe",
            ".test_shared.build",
        ] {
            assert!(modules.contains(&expected.to_string()), "{expected} missing from {modules:?}");
        }
        let files = HashMap::from([
            (FILE, source),
            ("tests/__init__.py", ""),
            ("tests/test_helpers.py", "EXPECTED = 3\n"),
            ("tests/test_other.py", ""),
            ("tests/test_maybe.py", ""),
            ("tests/test_shared.py", ""),
            ("pyproject.toml", PYPROJECT),
        ]);
        let read = reader(&files);
        let locked = regression_imports(FILE, &read, &declared()).unwrap();
        for path in ["tests/test_helpers.py", "tests/test_other.py", "tests/test_maybe.py", "tests/test_shared.py"] {
            assert!(locked.contains(path), "{path}: {locked:?}");
        }
        let changes =
            vec![("M".to_string(), "calc.py".to_string()), ("M".to_string(), "tests/test_helpers.py".to_string())];
        assert!(check_fix_changes_with(&changes, &rec(), &read, &read)
            .unwrap_err()
            .contains("frozen with the regression"));

        // Past the limit the guard reports that it cannot establish the inputs, never a silent pass.
        let many: String = (0..2001).map(|i| format!("import tests.test_h{i}\n")).collect();
        let helpers: Vec<String> = (0..2001).map(|i| format!("tests/test_h{i}.py")).collect();
        let mut wide: HashMap<&str, &str> = helpers.iter().map(|p| (p.as_str(), "")).collect();
        wide.insert(FILE, many.as_str());
        wide.insert("tests/__init__.py", "");
        wide.insert("pyproject.toml", PYPROJECT);
        let err = regression_imports(FILE, &reader(&wide), &declared()).unwrap_err();
        assert!(err.contains("cannot establish"), "{err}");
        let only_production = vec![("M".to_string(), "calc.py".to_string())];
        assert!(check_fix_changes_with(&only_production, &rec(), &reader(&wide), &reader(&wide))
            .unwrap_err()
            .contains("cannot establish"));
    }

    #[test]
    fn the_import_recorder_report_is_required_and_freezes_everything_but_production_code() {
        let report = "1 failed in 0.02s\nLYRA-LOADED calc.py\nLYRA-LOADED calc/util.py\nLYRA-LOADED src/calc/core.py\nLYRA-LOADED src/support/helper.py\nLYRA-LOADED tests/__init__.py\nLYRA-LOADED tests/test_helpers.py\nLYRA-LOADED tests/test_issue_7.py\nLYRA-LOADED conftest.py\nLYRA-LOADED support/__init__.py\nLYRA-LOADED support/helpers.py\nLYRA-LOADED helpers.py\nLYRA-LOADED .venv/lib/test_x.py\nLYRA-LOADED lib/site-packages/test_y.py\nLYRA-LOADED ../outside/test_z.py\nLYRA-READ fixtures/input.json\nLYRA-READ tests/data/case.txt\nLYRA-READ data/golden.bin\nLYRA-READ calc/table.csv\nLYRA-READ src/other/schema.json\nLYRA-READ pyproject.toml\nLYRA-READ pytest.ini\nLYRA-READ setup.cfg\nLYRA-READ tests/test_issue_7.py\nLYRA-READ tests/helpers.py\nLYRA-READ support/__pycache__/helpers.cpython-314.pyc\nLYRA-READ .pytest_cache/v/cache/nodeids\nLYRA-LOADED-END\n";
        assert_eq!(
            parse_loaded_modules(report, FILE, &declared()).unwrap(),
            [
                "calc/table.csv",
                "conftest.py",
                "data/golden.bin",
                "fixtures/input.json",
                "helpers.py",
                "pyproject.toml",
                "setup.cfg",
                "src/other/schema.json",
                "src/support/helper.py",
                "support/__init__.py",
                "support/helpers.py",
                "tests/__init__.py",
                "tests/data/case.txt",
                "tests/helpers.py",
                "tests/test_helpers.py"
            ],
            "declared `calc` modules (at the root or under `src/`) stay editable; undeclared modules under `src/` and \
             every non-source file the run read are frozen, root manifests the recorder reports as the \
             regression's own reads included; pytest configuration belongs to its guard, caches to nobody"
        );
        let err = parse_loaded_modules("1 failed in 0.02s\nLYRA-LOADED tests/test_helpers.py\n", FILE, &declared())
            .unwrap_err();
        assert!(err.contains("cannot establish"), "a run without the end marker fails closed: {err}");
        let spawned = report.replace(
            "LYRA-LOADED-END",
            "LYRA-SPAWN subprocess.Popen /usr/bin/python3\nLYRA-SPAWN os.system cat fixtures/input.json\nLYRA-LOADED-END",
        );
        let lost = report.replace("LYRA-LOADED-END", "LYRA-UNRESOLVED case.json\nLYRA-LOADED-END");
        let unresolved = parse_loaded_modules(&lost, FILE, &declared()).unwrap_err();
        assert!(unresolved.contains("`case.json`") && unresolved.contains("cannot establish"), "{unresolved}");
        let err = parse_loaded_modules(&spawned, FILE, &declared()).unwrap_err();
        for expected in [
            "child process",
            "`subprocess.Popen /usr/bin/python3`",
            "`os.system cat fixtures/input.json`",
            "in-process",
        ] {
            assert!(err.contains(expected), "{expected}: {err}");
        }
        // With nothing declared, production and support cannot be told apart: a visible error
        // naming the files and the remedy, never a silent choice.
        let err = parse_loaded_modules(report, FILE, &ProductionCode::default()).unwrap_err();
        for expected in ["`calc.py`", "`support/helpers.py`", "`helpers.py`", "declares no packages", "lyra.toml"] {
            assert!(err.contains(expected), "{expected}: {err}");
        }
        assert!(
            err.contains("`src/support/helper.py`") && !err.contains("`tests/") && !err.contains("`fixtures/input.json`"),
            "modules under src/ need a declaration too; the test tree and data never do (data is frozen either way): {err}"
        );
        // The recorder wraps the same flags and node id; classification ignores its report.
        let spec = PythonPytest.regression_command(&rec(), Path::new("/w")).unwrap();
        assert_eq!(spec.args[..2], ["-c".to_string(), PYTEST_RECORDER.to_string()]);
        assert_eq!(
            classify_pytest(&format!("{FAILED}LYRA-LOADED tests/test_helpers.py\nLYRA-LOADED-END\n"), ID_),
            RegressionOutcome::Failed
        );
    }

    /// End to end with real pytest: a regression that imports a test-named helper inline,
    /// conditionally and dynamically. The recorder reports what was loaded, Repro freezes it,
    /// and changing the helper (which would make the unchanged failing test pass) is rejected.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn inline_conditional_and_dynamic_imports_of_test_helpers_are_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("pytest.ini", "[pytest]\n");
        write("pyproject.toml", PYPROJECT);
        write("calc.py", "def add(a, b):\n    return a - b\n");
        write("tests/__init__.py", "");
        write("tests/test_helpers.py", "EXPECTED = 3\n");
        write("tests/test_offset.py", "OFFSET = 0\n");
        let test = "import importlib\nfrom calc import add\nif True: from tests.test_helpers import EXPECTED\n\ndef test_issue_7_fails():\n    offset = importlib.import_module('tests.' + 'test_' + 'offset').OFFSET\n    assert add(1, 2) == EXPECTED + offset, \"sum must be three\"\n";
        write(FILE, test);
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let spec = PythonPytest.reproduction_environment(PythonPytest.regression_command(&rec(), root).unwrap(), root);
        let run = proc::run(&spec, timeout, &cancel).await.unwrap();
        let out = run.combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");
        PythonPytest.confirm_failure(&run, &rec()).unwrap();
        let loaded = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap();
        assert!(loaded.contains(&"tests/test_helpers.py".to_string()), "inline conditional import: {loaded:?}");
        assert!(loaded.contains(&"tests/test_offset.py".to_string()), "computed dynamic import: {loaded:?}");
        assert!(!loaded.contains(&"calc.py".to_string()), "production stays editable: {loaded:?}");

        let mut record = rec();
        record.frozen_inputs = loaded;
        let frozen: HashMap<String, String> =
            [FILE, "tests/test_helpers.py", "tests/test_offset.py", "tests/__init__.py", "calc.py"]
                .into_iter()
                .map(|p| (p.to_string(), std::fs::read_to_string(root.join(p)).unwrap()))
                .collect();
        for (path, text) in [("tests/test_helpers.py", "EXPECTED = -1\n"), ("tests/test_offset.py", "OFFSET = -4\n")] {
            write(path, text);
            let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
            assert_eq!(
                classify_pytest(&out, ID_),
                RegressionOutcome::Passed,
                "the attack works without the lock: {out}"
            );
            let read_checkpoint = |p: &str| frozen.get(p).cloned();
            let read_now = |p: &str| std::fs::read_to_string(root.join(p)).ok();
            let err =
                check_fix_changes_with(&[("M".into(), path.into())], &record, &read_checkpoint, &read_now).unwrap_err();
            assert!(err.contains(path) && err.contains("test lock"), "{err}");
            write(path, &frozen[path]);
        }
    }

    #[test]
    fn production_code_is_read_from_packaging_metadata_and_the_repository_lyra_toml() {
        let files = HashMap::from([
            ("pyproject.toml", "[project]\nname = \"my-project\"\n[tool.setuptools]\npy-modules = [\"cli\"]\n[tool.setuptools.packages.find]\ninclude = [\"plugins*\", \"my_project.sub\"]\n[tool.poetry]\nname = \"poem\"\npackages = [{ include = \"verse\", from = \"src\" }]\n[tool.hatch.build.targets.wheel]\npackages = [\"src/hatched\"]\n[tool.flit.module]\nname = \"flat\"\n"),
            ("setup.cfg", "[metadata]\nname = cfg-dist\n[options]\npackages =\n    cfgpkg\n    cfgpkg.sub\npy_modules = single, other\n[options.extras_require]\ntest = pytest\n"),
            ("lyra.toml", "[verify]\ntest = \"python3 -m pytest\"\n[profiles.python-pytest]\npackages = [\"declared\", \"bad-name!\"]\n"),
            ("my_project/__init__.py", ""),
        ]);
        let production = ProductionCode::declared(&reader(&files));
        // `my_project` counts because `my_project/__init__.py` exists; the distribution names
        // `poem` and `cfg-dist` name nothing in the checkout and are not import names.
        let expected: BTreeSet<String> =
            ["my_project", "cli", "verse", "hatched", "flat", "cfgpkg", "single", "other", "declared"]
                .into_iter()
                .map(String::from)
                .collect();
        assert_eq!(production.names, expected);
        assert!(production.roots.contains(&(String::new(), "plugins*".to_string())), "{:?}", production.roots);
        assert_eq!(production.classify("plugins_extra/hook.py", "tests"), Classification::Production);
        // The reviewer's case: a flat layout whose distribution name differs from its import
        // package. The distribution name must not be taken for production code, or every real
        // fix to `libcode/` would be rejected as a change to frozen test support; without an
        // explicit declaration the repository is asked for one.
        let published = HashMap::from([
            ("pyproject.toml", "[project]\nname = \"published-project\"\n"),
            ("libcode/__init__.py", ""),
            ("libcode/main.py", "def main(): pass\n"),
        ]);
        let undeclared = ProductionCode::declared(&reader(&published));
        assert!(undeclared.names.is_empty(), "{:?}", undeclared.names);
        assert_eq!(undeclared.classify("libcode/main.py", "tests"), Classification::Unknown);
        let message = undeclared.unknown_message(&BTreeSet::from(["libcode/main.py".to_string()]));
        assert!(message.contains("`libcode/main.py`") && message.contains("[project] name` counts only"), "{message}");
        let mut declared = published.clone();
        declared.insert("setup.cfg", "[metadata]\nname = published-project\n[options]\npackages = libcode\n");
        let libcode = ProductionCode::declared(&reader(&declared));
        assert_eq!(libcode.names, BTreeSet::from(["libcode".to_string()]));
        assert_eq!(libcode.classify("libcode/main.py", "tests"), Classification::Production);
        // A distribution name spelled like its module, in either case, is enough on its own.
        let spelled = HashMap::from([("pyproject.toml", "[project]\nname = \"Calc-Tools\"\n"), ("calc_tools.py", "")]);
        assert_eq!(ProductionCode::declared(&reader(&spelled)).names, BTreeSet::from(["calc_tools".to_string()]));
        for (path, want) in [
            ("my_project/core.py", Classification::Production),
            ("cli.py", Classification::Production),
            // `src/` is a search path, not a declaration: only declared packages under it.
            ("src/anything/at_all.py", Classification::TestSupport),
            ("src/verse/poem.py", Classification::Production),
            ("support/helpers.py", Classification::TestSupport),
            ("helpers.py", Classification::TestSupport),
            ("tests/helpers.py", Classification::TestSupport),
            ("conftest.py", Classification::TestSupport),
        ] {
            assert_eq!(production.classify(path, "tests"), want, "{path}");
        }
        // Nothing declared: only `src/` and the test side are certain.
        let none = ProductionCode::declared(&|_: &str| None);
        assert_eq!(none.classify("src/pkg/a.py", "tests"), Classification::Unknown);
        assert_eq!(none.classify("tests/helpers.py", "tests"), Classification::TestSupport);
        assert_eq!(none.classify("support/helpers.py", "tests"), Classification::Unknown);
        assert_eq!(none.classify("calc.py", "tests"), Classification::Unknown);
        // A broken pyproject declares nothing rather than failing the parse silently elsewhere.
        assert!(ProductionCode::declared(&|p: &str| (p == "pyproject.toml").then(|| "[project\nname=".to_string()))
            .names
            .is_empty());
    }

    /// Declarations resolve to the directories they name: setuptools `packages.find` globs,
    /// `where`, `exclude` and `package-dir`; poetry `from`; hatch paths; `setup.cfg` `find:` and
    /// `package_dir`; paths in `lyra.toml`. Anything else stays frozen.
    #[test]
    fn package_declarations_resolve_to_their_actual_paths() {
        let classify = |files: &[(&str, &str)], path: &str| {
            let map: HashMap<&str, &str> = files.iter().copied().collect();
            let production = ProductionCode::declared(&reader(&map));
            production.classify(path, "tests")
        };
        use Classification::{Production, TestSupport, Unknown};
        let find_foo = "[tool.setuptools.packages.find]\ninclude = [\"foo*\"]\n";
        let poetry = "[tool.poetry]\npackages = [{ include = \"pkg\", from = \"lib\" }]\n";
        let find_lib = "[tool.setuptools.packages.find]\nwhere = [\"lib\"]\nexclude = [\"bench*\"]\n";
        let find_lib_globs =
            "[tool.setuptools.packages.find]\nwhere = [\"lib\"]\ninclude = [\"b*\", \"c*\"]\nexclude = [\"bench*\"]\n";
        let base = "[tool.setuptools]\npackages = [\"pkg\"]\npackage-dir = { \"\" = \"lib\" }\n";
        let mapped = "[tool.setuptools]\npackages = [\"pkg\"]\npackage-dir = { pkg = \"code/pkgsrc\" }\n";
        let hatch = "[tool.hatch.build.targets.wheel]\npackages = [\"lib/hatched\"]\n";
        let lyra = "[profiles.python-pytest]\npackages = [\"vendor/mine\"]\n";
        let cfg_find = "[options]\npackages = find:\npackage_dir =\n    =lib\n[options.packages.find]\nwhere = lib\ninclude = core*\n";
        let cfg_dir = "[options]\npackages = pkg\npackage_dir =\n    =lib\n";
        type Case<'a> = (Vec<(&'a str, &'a str)>, &'a str, Classification);
        let cases: Vec<Case> = vec![
            // The reviewer's two layouts.
            (vec![("pyproject.toml", find_foo)], "foobar/core.py", Production),
            (vec![("pyproject.toml", find_foo)], "bar/core.py", TestSupport),
            (vec![("pyproject.toml", poetry)], "lib/pkg/core.py", Production),
            (vec![("pyproject.toml", poetry)], "pkg/core.py", TestSupport),
            (vec![("pyproject.toml", poetry)], "lib/other/core.py", TestSupport),
            // find without include is as broad as `*`: it declares nothing, wherever it searches.
            (vec![("pyproject.toml", find_lib)], "lib/anything/x.py", Unknown),
            (vec![("pyproject.toml", find_lib)], "support/helpers.py", Unknown),
            // include globs with exclude below a non-root where.
            (vec![("pyproject.toml", find_lib_globs)], "lib/core/x.py", Production),
            (vec![("pyproject.toml", find_lib_globs)], "lib/benchmarks/x.py", TestSupport),
            (vec![("pyproject.toml", find_lib_globs)], "lib/support/helper.py", TestSupport),
            // At the root, find without include declares nothing (support code would become production).
            (vec![("pyproject.toml", "[tool.setuptools.packages.find]\n")], "support/helpers.py", Unknown),
            // package-dir: a base for the package list, and per-package directories.
            (vec![("pyproject.toml", base)], "lib/pkg/a.py", Production),
            (vec![("pyproject.toml", base)], "pkg/a.py", TestSupport),
            (vec![("pyproject.toml", mapped)], "code/pkgsrc/a.py", Production),
            // hatch and lyra.toml paths.
            (vec![("pyproject.toml", hatch)], "lib/hatched/a.py", Production),
            (vec![("lyra.toml", lyra)], "vendor/mine/a.py", Production),
            (vec![("lyra.toml", lyra)], "vendor/theirs/a.py", TestSupport),
            // setup.cfg find: and package_dir.
            (vec![("setup.cfg", cfg_find)], "lib/core_utils/a.py", Production),
            (vec![("setup.cfg", cfg_find)], "lib/other/a.py", TestSupport),
            (vec![("setup.cfg", cfg_dir)], "lib/pkg/a.py", Production),
        ];
        for (files, path, want) in cases {
            assert_eq!(classify(&files, path), want, "{path} with {files:?}");
        }
        assert!(glob_match("foo*", "foobar") && glob_match("f?o", "foo") && !glob_match("foo*", "barfoo"));
        assert!(
            glob_match("*", "")
                && glob_match("a*b*c", "aXbYc")
                && !glob_match("a*b*c", "aXbY")
                && glob_match("**", "x")
        );
        // Repeated stars with a non-matching suffix: bounded work (the recursive matcher was
        // exponential here), checked through the fix guard's own declaration path too.
        let adversarial = format!("{}b", "a*".repeat(60));
        let text = "a".repeat(4000);
        let started = std::time::Instant::now();
        assert!(!glob_match(&adversarial, &text));
        let pyproject = format!("[tool.setuptools.packages.find]\ninclude = [\"{adversarial}\", \"calc\"]\n");
        let files = HashMap::from([("pyproject.toml", pyproject.as_str())]);
        let production = ProductionCode::declared(&reader(&files));
        assert_eq!(production.classify(&format!("{text}/x.py"), "tests"), TestSupport);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());

        // A root pattern made only of wildcards would make every top-level directory, test
        // support included, production: it declares nothing and says so.
        let broad = "[tool.setuptools.packages.find]\ninclude = [\"*\"]\n";
        let files = HashMap::from([("pyproject.toml", broad)]);
        let production = ProductionCode::declared(&reader(&files));
        assert_eq!(production.classify("support/helper.py", "tests"), Unknown);
        let message = production.unknown_message(&BTreeSet::from(["support/helper.py".to_string()]));
        assert!(message.contains("`*` matches every package in its search path"), "{message}");
        let mixed = "[tool.setuptools.packages.find]\ninclude = [\"*\", \"?\", \"calc\"]\n";
        let files = HashMap::from([("pyproject.toml", mixed)]);
        let production = ProductionCode::declared(&reader(&files));
        assert_eq!(production.classify("support/helper.py", "tests"), TestSupport);
        assert_eq!(production.classify("calc/core.py", "tests"), Production);
        // The reviewer's case: `where = ["lib"]`, `include = ["*"]` (with `pythonpath = lib`).
        // Every package in `lib/`, test support next to them included: it declares nothing.
        let lib_star = "[tool.setuptools.packages.find]\nwhere = [\"lib\"]\ninclude = [\"*\"]\n";
        let files = HashMap::from([("pyproject.toml", lib_star)]);
        let production = ProductionCode::declared(&reader(&files));
        assert_eq!(production.classify("lib/support/helper.py", "tests"), Unknown);
        let message = production.unknown_message(&BTreeSet::from(["lib/support/helper.py".to_string()]));
        assert!(message.contains("`lib/*` matches every package in its search path"), "{message}");
        let named = "[tool.setuptools.packages.find]\nwhere = [\"lib\"]\ninclude = [\"*\", \"pkg\"]\n";
        let files = HashMap::from([("pyproject.toml", named)]);
        let production = ProductionCode::declared(&reader(&files));
        assert_eq!(production.classify("lib/support/helper.py", "tests"), TestSupport);
        assert_eq!(production.classify("lib/pkg/core.py", "tests"), Production);
        assert_eq!(join_dir("./lib/", "."), "lib");
        // A fix to the reviewer's `foobar/core.py` passes the lock.
        let files = HashMap::from([
            (FILE, "from foobar.core import f\n"),
            ("pyproject.toml", find_foo),
            ("foobar/core.py", ""),
        ]);
        let changes = vec![("M".to_string(), "foobar/core.py".to_string())];
        let mut record = rec();
        record.command = regression_line(ID_);
        assert!(check_fix_changes_with(&changes, &record, &reader(&files), &reader(&files)).is_ok());
    }

    #[test]
    fn support_loaded_outside_the_test_tree_is_locked_statically_and_through_frozen_inputs() {
        let frozen_test = "from calc import add\nfrom support.helpers import EXPECTED\n\ndef test_issue_7_fails():\n    assert add(1, 2) == EXPECTED, \"sum must be three\"\n";
        let files = HashMap::from([
            (FILE, frozen_test),
            ("pyproject.toml", PYPROJECT),
            ("calc.py", "def add(a, b): return a - b\n"),
            ("support/__init__.py", ""),
            ("support/helpers.py", "EXPECTED = 3\n"),
            ("support/unused.py", "X = 1\n"),
            ("tools/release.py", "Y = 1\n"),
        ]);
        let read = reader(&files);
        // Static layer: the helper is resolved from the import and locked, production is not.
        let locked = regression_imports(FILE, &read, &declared()).unwrap();
        assert_eq!(
            locked.iter().map(String::as_str).collect::<Vec<_>>(),
            ["support/__init__.py", "support/helpers.py"]
        );
        let changes = |path: &str| vec![("M".to_string(), "calc.py".to_string()), ("M".to_string(), path.to_string())];
        let err = check_fix_changes_with(&changes("support/helpers.py"), &rec(), &read, &read).unwrap_err();
        assert!(err.contains("support/helpers.py") && err.contains("test lock"), "{err}");
        let err = check_fix_changes_with(&changes("tools/release.py"), &rec(), &read, &read).unwrap_err();
        assert!(err.contains("not declared production code"), "undeclared code is frozen too: {err}");

        // Recorder layer: what the run loaded is frozen, and its whole tree with it, so nothing
        // can be added or changed next to the frozen helper even when the static layer sees no import.
        let mut record = rec();
        record.frozen_inputs = vec!["support/helpers.py".into()];
        let silent = HashMap::from([
            (FILE, "import importlib\nfrom calc import add\n\ndef test_issue_7_fails():\n    assert add(1, 2) == importlib.import_module('sup' + 'port.helpers').EXPECTED\n"),
            ("pyproject.toml", PYPROJECT),
            ("calc.py", "def add(a, b): return a - b\n"),
            ("support/__init__.py", ""),
            ("support/helpers.py", "EXPECTED = 3\n"),
        ]);
        let read = reader(&silent);
        for (status, path) in [
            ("M", "support/helpers.py"),
            ("A", "support/shadow.py"),
            ("M", "support/__init__.py"),
            ("D", "support/unused.py"),
        ] {
            let changes = vec![("M".to_string(), "calc.py".to_string()), (status.to_string(), path.to_string())];
            let err = check_fix_changes_with(&changes, &record, &read, &read).unwrap_err();
            assert!(err.contains(path) && err.contains("test lock"), "{status} {path}: {err}");
        }
        assert!(check_fix_changes_with(&[("M".to_string(), "calc.py".to_string())], &record, &read, &read).is_ok());

        // Without a declaration the fix is rejected visibly instead of guessing which is which.
        let undeclared: HashMap<&str, &str> =
            files.iter().filter(|(k, _)| **k != "pyproject.toml").map(|(k, v)| (*k, *v)).collect();
        let read = reader(&undeclared);
        let err = check_fix_changes_with(&changes("tools/release.py"), &rec(), &read, &read).unwrap_err();
        assert!(err.contains("declares no packages") && err.contains("`calc.py`"), "{err}");
    }

    #[test]
    fn the_recorded_command_is_one_copyable_line_while_the_recorder_runs() {
        let line = regression_line(ID_);
        assert_eq!(
            line,
            format!(
                "{PYTHON} -m pytest -p no:cacheprovider -o addopts= --rootdir=. --confcutdir=. --color=no -rA {ID_}"
            )
        );
        assert!(
            !line.contains('\n') && line.chars().all(|c| c.is_ascii_alphanumeric() || " -_./:=".contains(c)),
            "valid as typed in sh and PowerShell: {line}"
        );
        let spec = PythonPytest.regression_command(&rec(), Path::new("/w")).unwrap();
        assert_eq!(spec.args[1], PYTEST_RECORDER, "what runs is still the recorder");
        assert_eq!(spec.display(), line, "what is shown is the plain command");
        assert_eq!(
            PythonPytest.command_environment(spec, Path::new("/env")).display(),
            line,
            "the environment keeps the shown line"
        );
        let steps = PythonPytest.verification_steps(&VerifyCommands::default(), &rec(), Path::new("/w")).unwrap();
        assert!(steps.iter().all(|s| !s.spec.display().contains('\n')), "every verify step fits one table cell");
    }

    /// End to end with real pytest, the reviewer's scenario: the regression imports shared
    /// test infrastructure outside the test tree (`support/helpers.py`). Changing the helper's
    /// expected value makes the unchanged failing test pass; both the recorder layer and the
    /// static layer reject the change, and so does adding a module next to it.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn a_shared_helper_outside_the_test_tree_is_frozen_with_the_regression() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("pytest.ini", "[pytest]\n");
        write("pyproject.toml", PYPROJECT);
        write("calc.py", "def add(a, b):\n    return a - b\n");
        write("support/__init__.py", "");
        write("support/helpers.py", "EXPECTED = 3\n");
        write(FILE, "from calc import add\nfrom support.helpers import EXPECTED\n\ndef test_issue_7_fails():\n    assert add(1, 2) == EXPECTED, \"sum must be three\"\n");
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let spec = PythonPytest.reproduction_environment(PythonPytest.regression_command(&rec(), root).unwrap(), root);
        let run = proc::run(&spec, timeout, &cancel).await.unwrap();
        let out = run.combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");
        PythonPytest.confirm_failure(&run, &rec()).unwrap();
        let loaded = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap();
        assert_eq!(loaded, ["support/__init__.py", "support/helpers.py"], "production `calc` is not frozen");

        let frozen: HashMap<String, String> =
            [FILE, "pyproject.toml", "calc.py", "support/__init__.py", "support/helpers.py"]
                .into_iter()
                .map(|p| (p.to_string(), std::fs::read_to_string(root.join(p)).unwrap()))
                .collect();
        write("support/helpers.py", "EXPECTED = -1\n");
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Passed, "the attack works without the lock: {out}");
        let read_checkpoint = |p: &str| frozen.get(p).cloned();
        let read_now = |p: &str| std::fs::read_to_string(root.join(p)).ok();
        let change = vec![("M".to_string(), "support/helpers.py".to_string())];
        let mut with_recorder = rec();
        with_recorder.frozen_inputs = loaded;
        for record in [&with_recorder, &rec()] {
            let err = check_fix_changes_with(&change, record, &read_checkpoint, &read_now).unwrap_err();
            assert!(err.contains("support/helpers.py") && err.contains("test lock"), "{err}");
        }
        write("support/helpers.py", &frozen["support/helpers.py"]);
        write("support/shadow.py", "EXPECTED = -1\n");
        let added = vec![("M".to_string(), "calc.py".to_string()), ("A".to_string(), "support/shadow.py".to_string())];
        let err = check_fix_changes_with(&added, &with_recorder, &read_checkpoint, &read_now).unwrap_err();
        assert!(err.contains("support/shadow.py") && err.contains("not declared production code"), "{err}");
    }

    /// End to end with real pytest, the reviewer's fixture-only bypass: the regression reads a
    /// tracked `fixtures/input.json` that existed before Repro, through `open` and through
    /// `pathlib`, plus a data file inside the test tree. The recorder reports the reads, so a
    /// fix that only swaps the triggering input for a benign one is rejected; a repository that
    /// cannot say what its production code is fails closed at S2 instead of guessing.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn pre_existing_fixture_data_read_by_the_regression_is_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("pytest.ini", "[pytest]\n");
        write("pyproject.toml", PYPROJECT);
        write("calc.py", "import json\n\ndef add(a, b):\n    return a - b\n\ndef load(path):\n    with open(path) as f:\n        return json.load(f)\n");
        write("fixtures/input.json", "{\"a\": 1, \"b\": 2}\n");
        write("data/expected.txt", "3\n");
        write("tests/cases/limit.txt", "0\n");
        write(
            FILE,
            "import os, pathlib\nfrom calc import add, load\n\nHERE = os.path.dirname(__file__)\n\ndef test_issue_7_fails():\n    case = load(\"fixtures/input.json\")\n    expected = int(pathlib.Path(\"data/expected.txt\").read_text())\n    limit = int(open(os.path.join(HERE, \"cases/limit.txt\")).read())\n    assert add(case[\"a\"], case[\"b\"]) == expected + limit, \"sum must be three\"\n",
        );
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let spec = PythonPytest.reproduction_environment(PythonPytest.regression_command(&rec(), root).unwrap(), root);
        let run = proc::run(&spec, timeout, &cancel).await.unwrap();
        let out = run.combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");
        PythonPytest.confirm_failure(&run, &rec()).unwrap();
        let loaded = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap();
        assert_eq!(
            loaded,
            ["data/expected.txt", "fixtures/input.json", "tests/cases/limit.txt"],
            "every read input is frozen, production `calc` is not: {out}"
        );

        // Without a declaration of the production code, the module `calc.py` cannot be
        // classified: S2 stops. Data outside `src/` is frozen either way and needs no declaration.
        std::fs::remove_file(root.join("pyproject.toml")).unwrap();
        let err = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap_err();
        assert!(err.contains("`calc.py`") && err.contains("declares no packages"), "{err}");
        assert!(!err.contains("`fixtures/input.json`"), "{err}");
        write("pyproject.toml", PYPROJECT);

        let frozen: HashMap<String, String> =
            [FILE, "pyproject.toml", "calc.py", "fixtures/input.json", "data/expected.txt", "tests/cases/limit.txt"]
                .into_iter()
                .map(|p| (p.to_string(), std::fs::read_to_string(root.join(p)).unwrap()))
                .collect();
        let read_checkpoint = |p: &str| frozen.get(p).cloned();
        let read_now = |p: &str| std::fs::read_to_string(root.join(p)).ok();
        let mut record = rec();
        record.frozen_inputs = loaded;
        for (path, benign) in [
            ("fixtures/input.json", "{\"a\": 4, \"b\": 1}\n"),
            ("data/expected.txt", "-1\n"),
            ("tests/cases/limit.txt", "-4\n"),
        ] {
            write(path, benign);
            let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
            assert_eq!(
                classify_pytest(&out, ID_),
                RegressionOutcome::Passed,
                "{path}: the attack works without the lock: {out}"
            );
            let change = vec![("M".to_string(), path.to_string())];
            let err = check_fix_changes_with(&change, &record, &read_checkpoint, &read_now).unwrap_err();
            assert!(err.contains(path) && err.contains("test lock"), "{path}: {err}");
            write(path, &frozen[path]);
        }
        // Nothing can be added next to a frozen input either, and the real fix is accepted.
        write("fixtures/other.json", "{}\n");
        let added = vec![("A".to_string(), "fixtures/other.json".to_string())];
        let err = check_fix_changes_with(&added, &record, &read_checkpoint, &read_now).unwrap_err();
        assert!(err.contains("frozen with the regression"), "{err}");
        std::fs::remove_file(root.join("fixtures/other.json")).unwrap();
        write("calc.py", &frozen["calc.py"].replace("a - b", "a + b"));
        let fix = vec![("M".to_string(), "calc.py".to_string())];
        check_fix_changes_with(&fix, &record, &read_checkpoint, &read_now).unwrap();
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Passed, "{out}");
    }

    /// End to end with real pytest, the reviewer's two follow-up forms: a fixture read through a
    /// read/write open (`r+`, `a+`, `os.open(O_RDWR)`) is recorded and frozen like a plain read,
    /// while truncating and write-only opens are not reads; a regression that reads the fixture
    /// through a child process cannot have its inputs established and is rejected at S2.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn read_write_opens_are_reads_and_child_processes_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("pytest.ini", "[pytest]\n");
        write("pyproject.toml", PYPROJECT);
        write("calc.py", "def add(a, b):\n    return a - b\n");
        write("fixtures/input.json", "1 2\n");
        write("fixtures/append.txt", "3\n");
        write("fixtures/raw.txt", "0\n");
        write("fixtures/scratch.txt", "old\n");
        write("fixtures/log.txt", "old\n");
        write(
            FILE,
            "import os\nfrom calc import add\n\ndef test_issue_7_fails():\n    with open(\"fixtures/input.json\", \"r+\") as f:\n        a, b = map(int, f.read().split())\n    with open(\"fixtures/append.txt\", \"a+\") as f:\n        f.seek(0)\n        expected = int(f.read())\n    fd = os.open(\"fixtures/raw.txt\", os.O_RDWR)\n    limit = int(os.read(fd, 16))\n    os.close(fd)\n    with open(\"fixtures/scratch.txt\", \"w+\") as f:\n        f.write(\"new\")\n    with open(\"fixtures/log.txt\", \"a\") as f:\n        f.write(\"x\")\n    assert add(a, b) == expected + limit, \"sum must be three\"\n",
        );
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let spec = PythonPytest.reproduction_environment(PythonPytest.regression_command(&rec(), root).unwrap(), root);
        let run = proc::run(&spec, timeout, &cancel).await.unwrap();
        let out = run.combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");
        let loaded = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap();
        assert_eq!(
            loaded,
            ["fixtures/append.txt", "fixtures/input.json", "fixtures/raw.txt"],
            "r+, a+ and O_RDWR consume existing data; w+ and a do not: {out}"
        );
        // Restore what the run wrote, as the S2 checkpoint holds it, then attack the r+ fixture.
        write("fixtures/scratch.txt", "old\n");
        write("fixtures/log.txt", "old\n");
        let frozen: HashMap<String, String> =
            [FILE, "pyproject.toml", "calc.py", "fixtures/input.json", "fixtures/append.txt", "fixtures/raw.txt"]
                .into_iter()
                .map(|p| (p.to_string(), std::fs::read_to_string(root.join(p)).unwrap()))
                .collect();
        let read_checkpoint = |p: &str| frozen.get(p).cloned();
        let read_now = |p: &str| std::fs::read_to_string(root.join(p)).ok();
        let mut record = rec();
        record.frozen_inputs = loaded;
        write("fixtures/input.json", "4 1\n");
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Passed, "the attack works without the lock: {out}");
        let change = vec![("M".to_string(), "fixtures/input.json".to_string())];
        let err = check_fix_changes_with(&change, &record, &read_checkpoint, &read_now).unwrap_err();
        assert!(err.contains("fixtures/input.json") && err.contains("test lock"), "{err}");
        write("fixtures/input.json", &frozen["fixtures/input.json"]);

        // A child process reads the fixture: the parent cannot see that read, so S2 stops.
        write(
            FILE,
            "import subprocess, sys\nfrom calc import add\n\ndef test_issue_7_fails():\n    out = subprocess.run([sys.executable, \"-c\", \"print(open('fixtures/input.json').read())\"], capture_output=True, text=True, check=True).stdout\n    a, b = map(int, out.split())\n    assert add(a, b) == 3, \"sum must be three\"\n",
        );
        let run = proc::run(&spec, timeout, &cancel).await.unwrap();
        let out = run.combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");
        assert!(!out.contains("LYRA-READ fixtures/input.json"), "precondition: the parent never sees the read: {out}");
        let err = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap_err();
        assert!(err.contains("child process") && err.contains("subprocess.Popen"), "{err}");
        // os.system is caught the same way.
        write(
            FILE,
            "import os\nfrom calc import add\n\ndef test_issue_7_fails():\n    os.system(\"cat fixtures/input.json > /dev/null\")\n    assert add(1, 2) == 3, \"sum must be three\"\n",
        );
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        let err = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap_err();
        assert!(err.contains("`os.system cat fixtures/input.json > /dev/null`"), "{err}");
    }

    /// End to end with real pytest, the reviewer's two cases for the allowlist lock: a fixture
    /// inside `src/` (under a package's own `tests/fixtures/`) and a pre-existing
    /// `fixtures/pyproject.toml` the regression reads as data. Changing either makes the
    /// unchanged failing test pass; the lock rejects modification, deletion, a rename (a
    /// deletion plus an addition) and an addition next to them, whether or not the recorder saw
    /// the read; the real fix in the declared package is accepted.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn fixtures_inside_src_and_manifest_named_fixtures_are_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("pytest.ini", "[pytest]\n");
        write("pyproject.toml", "[project]\nname = \"published-calc\"\n[tool.setuptools]\npackages = [\"calc\"]\n");
        write("src/calc/__init__.py", "def add(a, b):\n    return a - b\n");
        write("src/calc/tests/fixtures/input.json", "[1, 2]\n");
        write("fixtures/pyproject.toml", "[project]\nversion = \"3\"\n");
        write(
            FILE,
            "import json, pathlib, tomllib\nfrom calc import add\n\ndef test_issue_7_fails():\n    a, b = json.loads(pathlib.Path(\"src/calc/tests/fixtures/input.json\").read_text())\n    expected = int(tomllib.loads(pathlib.Path(\"fixtures/pyproject.toml\").read_text())[\"project\"][\"version\"])\n    assert add(a, b) == expected, \"sum must be three\"\n",
        );
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let spec = PythonPytest.reproduction_environment(PythonPytest.regression_command(&rec(), root).unwrap(), root);
        let run = proc::run(&spec, timeout, &cancel).await.unwrap();
        let out = run.combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");
        let loaded = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap();
        assert_eq!(
            loaded,
            ["fixtures/pyproject.toml", "src/calc/tests/fixtures/input.json"],
            "a manifest-named fixture is data, and test fixtures inside src/ are not production: {out}"
        );

        let paths =
            ["pyproject.toml", "src/calc/__init__.py", "src/calc/tests/fixtures/input.json", "fixtures/pyproject.toml"];
        let frozen: HashMap<String, String> = std::iter::once(FILE)
            .chain(paths)
            .map(|p| (p.to_string(), std::fs::read_to_string(root.join(p)).unwrap()))
            .collect();
        let read_checkpoint = |p: &str| frozen.get(p).cloned();
        let read_now = |p: &str| std::fs::read_to_string(root.join(p)).ok();
        let with_recorder = ReproRecord { frozen_inputs: loaded, ..rec() };
        for (path, benign) in [
            ("src/calc/tests/fixtures/input.json", "[4, 1]\n"),
            ("fixtures/pyproject.toml", "[project]\nversion = \"-1\"\n"),
        ] {
            write(path, benign);
            let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
            assert_eq!(
                classify_pytest(&out, ID_),
                RegressionOutcome::Passed,
                "{path}: the attack works without the lock: {out}"
            );
            let modified = vec![("M".to_string(), path.to_string())];
            // Rejected through the frozen inputs and, independently, through the allowlist alone.
            for record in [&with_recorder, &rec()] {
                let err = check_fix_changes_with(&modified, record, &read_checkpoint, &read_now).unwrap_err();
                assert!(err.contains(path), "{path}: {err}");
            }
            write(path, &frozen[path]);

            // Deletion, rename (deletion plus addition) and addition next to it.
            std::fs::remove_file(root.join(path)).unwrap();
            let renamed = format!("{path}.moved");
            write(&renamed, &frozen[path]);
            let sibling = format!("{}/extra.json", path.rsplit_once('/').unwrap().0);
            write(&sibling, "{}\n");
            for changes in [
                vec![("D".to_string(), path.to_string())],
                vec![("D".to_string(), path.to_string()), ("A".to_string(), renamed.clone())],
                vec![("A".to_string(), sibling.clone())],
            ] {
                for record in [&with_recorder, &rec()] {
                    let err = check_fix_changes_with(&changes, record, &read_checkpoint, &read_now).unwrap_err();
                    let last = &changes.last().unwrap().1;
                    assert!(err.contains(last.as_str()), "{changes:?}: {err}");
                }
            }
            std::fs::remove_file(root.join(&renamed)).unwrap();
            std::fs::remove_file(root.join(&sibling)).unwrap();
            write(path, &frozen[path]);
        }

        // The real fix, in the declared package, is accepted and passes.
        write("src/calc/__init__.py", "def add(a, b):\n    return a + b\n");
        let fix = vec![("M".to_string(), "src/calc/__init__.py".to_string())];
        check_fix_changes_with(&fix, &with_recorder, &read_checkpoint, &read_now).unwrap();
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Passed, "{out}");
    }

    /// End to end with real pytest, the reviewer's ambiguous test data under `src/`: the
    /// regression reads `src/app/data/test_vector.json`, an ordinary package data directory
    /// with no `tests` or `fixtures` in its path. Changing it makes the unchanged failing test
    /// pass; the recorder freezes it, and the allowlist rejects it on its own because only
    /// source code may change inside production code. The real fix is accepted.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn test_data_inside_production_code_is_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("pytest.ini", "[pytest]\n");
        write("pyproject.toml", "[tool.setuptools]\npackages = [\"app\"]\n");
        write("src/app/__init__.py", "def add(a, b):\n    return a - b\n");
        write("src/app/data/test_vector.json", "[1, 2, 3]\n");
        write(
            FILE,
            "import json, pathlib\nfrom app import add\n\ndef test_issue_7_fails():\n    a, b, expected = json.loads(pathlib.Path(\"src/app/data/test_vector.json\").read_text())\n    assert add(a, b) == expected, \"sum must be three\"\n",
        );
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let spec = PythonPytest.reproduction_environment(PythonPytest.regression_command(&rec(), root).unwrap(), root);
        let run = proc::run(&spec, timeout, &cancel).await.unwrap();
        let out = run.combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");
        let loaded = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap();
        assert_eq!(loaded, ["src/app/data/test_vector.json"], "{out}");

        let frozen: HashMap<String, String> =
            [FILE, "pyproject.toml", "src/app/__init__.py", "src/app/data/test_vector.json"]
                .into_iter()
                .map(|p| (p.to_string(), std::fs::read_to_string(root.join(p)).unwrap()))
                .collect();
        let read_checkpoint = |p: &str| frozen.get(p).cloned();
        let read_now = |p: &str| std::fs::read_to_string(root.join(p)).ok();
        let with_recorder = ReproRecord { frozen_inputs: loaded, ..rec() };
        write("src/app/data/test_vector.json", "[4, 1, 3]\n");
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Passed, "the attack works without the lock: {out}");
        let modified = vec![("M".to_string(), "src/app/data/test_vector.json".to_string())];
        let err = check_fix_changes_with(&modified, &with_recorder, &read_checkpoint, &read_now).unwrap_err();
        assert!(err.contains("src/app/data/test_vector.json") && err.contains("locked"), "{err}");
        let err = check_fix_changes_with(&modified, &rec(), &read_checkpoint, &read_now).unwrap_err();
        assert!(err.contains("only source code may change"), "the allowlist alone rejects it: {err}");
        // Renamed or a sibling added: rejected too.
        let added = vec![("A".to_string(), "src/app/data/other.json".to_string())];
        assert!(check_fix_changes_with(&added, &rec(), &read_checkpoint, &read_now).is_err());
        write("src/app/data/test_vector.json", &frozen["src/app/data/test_vector.json"]);

        write("src/app/__init__.py", "def add(a, b):\n    return a + b\n");
        let fix = vec![("M".to_string(), "src/app/__init__.py".to_string())];
        check_fix_changes_with(&fix, &with_recorder, &read_checkpoint, &read_now).unwrap();
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Passed, "{out}");
    }

    /// End to end with real pytest, the reviewer's broad-declaration bypass: with
    /// `include = ["*"]` the regression imports `EXPECTED` from `support/helper.py`. A
    /// wildcard-only root pattern declares nothing, so S2 stops visibly; once the repository
    /// names its package next to it, the helper is frozen and editing it, which would make the
    /// unchanged failing test pass, is rejected.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn a_wildcard_package_declaration_does_not_unfreeze_test_support() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("pytest.ini", "[pytest]\n");
        write("pyproject.toml", "[tool.setuptools.packages.find]\ninclude = [\"*\"]\n");
        write("calc/__init__.py", "def add(a, b):\n    return a - b\n");
        write("support/__init__.py", "");
        write("support/helper.py", "EXPECTED = 3\n");
        write(FILE, "from calc import add\nfrom support.helper import EXPECTED\n\ndef test_issue_7_fails():\n    assert add(1, 2) == EXPECTED, \"sum must be three\"\n");
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let spec = PythonPytest.reproduction_environment(PythonPytest.regression_command(&rec(), root).unwrap(), root);
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");
        let err = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap_err();
        assert!(
            err.contains("`support/helper.py`") && err.contains("`*` matches every package in its search path"),
            "{err}"
        );

        write("pyproject.toml", "[tool.setuptools.packages.find]\ninclude = [\"*\", \"calc\"]\n");
        let loaded = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap();
        assert_eq!(loaded, ["support/__init__.py", "support/helper.py"], "{out}");
        let frozen: HashMap<String, String> =
            [FILE, "pyproject.toml", "calc/__init__.py", "support/__init__.py", "support/helper.py"]
                .into_iter()
                .map(|p| (p.to_string(), std::fs::read_to_string(root.join(p)).unwrap()))
                .collect();
        let read_checkpoint = |p: &str| frozen.get(p).cloned();
        let read_now = |p: &str| std::fs::read_to_string(root.join(p)).ok();
        write("support/helper.py", "EXPECTED = -1\n");
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Passed, "the attack works without the lock: {out}");
        let change = vec![("M".to_string(), "support/helper.py".to_string())];
        for record in [&ReproRecord { frozen_inputs: loaded.clone(), ..rec() }, &rec()] {
            let err = check_fix_changes_with(&change, record, &read_checkpoint, &read_now).unwrap_err();
            assert!(err.contains("support/helper.py"), "{err}");
        }
        write("support/helper.py", &frozen["support/helper.py"]);
        write("calc/__init__.py", "def add(a, b):\n    return a + b\n");
        let fix = vec![("M".to_string(), "calc/__init__.py".to_string())];
        check_fix_changes_with(&fix, &ReproRecord { frozen_inputs: loaded, ..rec() }, &read_checkpoint, &read_now)
            .unwrap();
    }

    /// The reviewer's follow-up: the same bypass below a non-root search path. With
    /// `where = ["lib"]`, `include = ["*"]` and pytest's `pythonpath = lib`, the regression
    /// imports `lib/support/helper.py` under a computed name. `lib/*` declares nothing, so S2
    /// stops; with the package named, the helper is frozen and the edit that would make the
    /// unchanged failing test pass is rejected.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn a_wildcard_below_a_search_path_does_not_unfreeze_test_support() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("pytest.ini", "[pytest]\npythonpath = lib\n");
        write("pyproject.toml", "[tool.setuptools.packages.find]\nwhere = [\"lib\"]\ninclude = [\"*\"]\n");
        write("lib/calc/__init__.py", "def add(a, b):\n    return a - b\n");
        write("lib/support/__init__.py", "");
        write("lib/support/helper.py", "EXPECTED = 3\n");
        write(
            FILE,
            "import importlib\nfrom calc import add\n\ndef test_issue_7_fails():\n    expected = importlib.import_module(\"sup\" + \"port.helper\").EXPECTED\n    assert add(1, 2) == expected, \"sum must be three\"\n",
        );
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let spec = PythonPytest.reproduction_environment(PythonPytest.regression_command(&rec(), root).unwrap(), root);
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");
        let err = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap_err();
        assert!(
            err.contains("`lib/support/helper.py`") && err.contains("`lib/*` matches every package in its search path"),
            "{err}"
        );

        write("pyproject.toml", "[tool.setuptools.packages.find]\nwhere = [\"lib\"]\ninclude = [\"*\", \"calc\"]\n");
        let loaded = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap();
        assert_eq!(loaded, ["lib/support/__init__.py", "lib/support/helper.py"], "{out}");
        let paths = [
            FILE,
            "pytest.ini",
            "pyproject.toml",
            "lib/calc/__init__.py",
            "lib/support/__init__.py",
            "lib/support/helper.py",
        ];
        let frozen: HashMap<String, String> =
            paths.into_iter().map(|p| (p.to_string(), std::fs::read_to_string(root.join(p)).unwrap())).collect();
        let read_checkpoint = |p: &str| frozen.get(p).cloned();
        let read_now = |p: &str| std::fs::read_to_string(root.join(p)).ok();
        write("lib/support/helper.py", "EXPECTED = -1\n");
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Passed, "the attack works without the lock: {out}");
        let change = vec![("M".to_string(), "lib/support/helper.py".to_string())];
        for record in [&ReproRecord { frozen_inputs: loaded.clone(), ..rec() }, &rec()] {
            let err = check_fix_changes_with(&change, record, &read_checkpoint, &read_now).unwrap_err();
            assert!(err.contains("lib/support/helper.py"), "{err}");
        }
        write("lib/support/helper.py", &frozen["lib/support/helper.py"]);
        write("lib/calc/__init__.py", "def add(a, b):\n    return a + b\n");
        let fix = vec![("M".to_string(), "lib/calc/__init__.py".to_string())];
        check_fix_changes_with(&fix, &ReproRecord { frozen_inputs: loaded, ..rec() }, &read_checkpoint, &read_now)
            .unwrap();
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Passed, "{out}");
    }

    /// Real Python, the reviewer's false-failure cases for the `imports` step: a shared
    /// `google.*` namespace whose other portion a dependency installs elsewhere passes, as do a
    /// stdlib import next to a nested `json.py`, a quoted word next to a nested module and a
    /// root `__main__.py`; a regular `google` package elsewhere that shadows the namespace still
    /// fails by name. (A project module named like one the interpreter imports at start-up,
    /// such as a root `types.py`, stops Python itself from starting, so it never reaches here.)
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn the_imports_step_accepts_shared_namespaces_and_startup_module_names() {
        let dir = tempfile::tempdir().unwrap();
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let write = |path: std::path::PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        let checkout = dir.path().join("checkout");
        write(checkout.join("src/google/cloud/mylib/__init__.py"), "def answer():\n    return 42\n");
        write(checkout.join("__main__.py"), "print('cli')\n");
        write(checkout.join("src/mylib_tools/serializers/json.py"), "");
        write(checkout.join("pytest.ini"), "[pytest]\n");
        write(
            checkout.join(FILE),
            "import json\nfrom google.cloud.mylib import answer\n\ndef test_issue_7_fails():\n    assert answer() == 42, \"status\"\n",
        );
        let external = dir.path().join("site");
        write(external.join("google/auth/__init__.py"), "");

        let venv = dir.path().join("venv");
        let made = proc::run(
            &CommandSpec::new(PYTHON, dir.path()).args(["-m", "venv", "--without-pip"]).arg(venv.to_string_lossy()),
            timeout,
            &cancel,
        )
        .await
        .unwrap();
        assert!(made.success(), "{}", made.combined());
        let venv_python = if cfg!(windows) { venv.join("Scripts/python.exe") } else { venv.join("bin/python") };
        let purelib = proc::run(
            &CommandSpec::new(venv_python.to_string_lossy(), dir.path())
                .args(["-c", "import sysconfig; print(sysconfig.get_paths()['purelib'])"]),
            timeout,
            &cancel,
        )
        .await
        .unwrap();
        let purelib = std::path::PathBuf::from(purelib.stdout.trim());
        write(purelib.join("external.pth"), &format!("{}\n", external.display()));
        let with_venv = |mut spec: CommandSpec| {
            spec.program = venv_python.to_string_lossy().into_owned();
            spec
        };

        let steps = PythonPytest.verification_steps(&VerifyCommands::default(), &rec(), &checkout).unwrap();
        assert_eq!(steps[0].name, "imports");
        let names = steps[0].spec.stdin.clone().unwrap();
        assert_eq!(names, "google\nmylib_tools\n", "no json, status or __main__");
        let env_root = dir.path().join("env");
        let run = || async {
            proc::run(&with_venv(PythonPytest.command_environment(steps[0].spec.clone(), &env_root)), timeout, &cancel)
                .await
                .unwrap()
        };
        let check = run().await;
        assert!(check.success(), "shared namespace and start-up names pass: {}", check.combined());

        // A regular `google` package elsewhere shadows the checkout's namespace: still caught.
        write(external.join("google/__init__.py"), "");
        let check = run().await;
        assert!(
            !check.success() && check.stdout.contains("ERROR: google does not resolve inside the checkout"),
            "{}",
            check.combined()
        );
    }

    /// End to end with real pytest: a test placed next to the code (`textkit/test_issue_7.py`,
    /// no `tests/` directory) does not turn its package into test code. The recorder freezes
    /// nothing from `textkit/`, and the fix to `textkit/lists.py` is accepted and passes.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn a_test_next_to_the_code_leaves_its_package_editable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        const COLOCATED: &str = "textkit/test_issue_7.py";
        const COLOCATED_ID: &str = "textkit/test_issue_7.py::test_issue_7_fails";
        write("pytest.ini", "[pytest]\n");
        write("pyproject.toml", "[tool.setuptools]\npackages = [\"textkit\"]\n");
        write("textkit/__init__.py", "from .lists import first\n");
        write("textkit/lists.py", "def first(items):\n    return items[-1]\n");
        write("textkit/test_lists.py", "def test_existing():\n    assert True\n");
        write(COLOCATED, "from textkit.lists import first\n\ndef test_issue_7_fails():\n    assert first([1, 2]) == 1, \"first must return the first item\"\n");
        let record = ReproRecord {
            test_file: COLOCATED.into(),
            test_name: COLOCATED_ID.into(),
            command: regression_line(COLOCATED_ID),
            ..rec()
        };
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let spec = PythonPytest.reproduction_environment(PythonPytest.regression_command(&record, root).unwrap(), root);
        let run = proc::run(&spec, timeout, &cancel).await.unwrap();
        let out = run.combined();
        assert_eq!(classify_pytest(&out, COLOCATED_ID), RegressionOutcome::Failed, "{out}");
        let loaded = PythonPytest.loaded_test_inputs(&out, &record, root).unwrap();
        assert!(loaded.iter().all(|p| !p.starts_with("textkit/") || p.contains("test_")), "{loaded:?}");
        assert!(!loaded.contains(&"textkit/lists.py".to_string()), "{loaded:?}");

        let frozen: HashMap<String, String> = [
            COLOCATED,
            "pytest.ini",
            "pyproject.toml",
            "textkit/__init__.py",
            "textkit/lists.py",
            "textkit/test_lists.py",
        ]
        .into_iter()
        .map(|p| (p.to_string(), std::fs::read_to_string(root.join(p)).unwrap()))
        .collect();
        let read_checkpoint = |p: &str| frozen.get(p).cloned();
        let read_now = |p: &str| std::fs::read_to_string(root.join(p)).ok();
        write("textkit/lists.py", "def first(items):\n    return items[0]\n");
        let record = ReproRecord { frozen_inputs: loaded, ..record };
        let fix = vec![("M".to_string(), "textkit/lists.py".to_string())];
        check_fix_changes_with(&fix, &record, &read_checkpoint, &read_now).unwrap();
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, COLOCATED_ID), RegressionOutcome::Passed, "{out}");
        // The neighbouring test module stays frozen.
        let change = vec![("M".to_string(), "textkit/test_lists.py".to_string())];
        assert!(check_fix_changes_with(&change, &record, &read_checkpoint, &read_now).is_err());
    }

    /// The lists that must agree stay in agreement: every pytest configuration file is a
    /// manifest (so it reaches the context snapshot) and generated directories are one list.
    #[test]
    fn configuration_and_generated_lists_agree() {
        for name in PYTEST_CONFIG_FILES {
            assert!(PythonPytest.manifest_names().contains(&name), "{name} missing from manifest_names");
        }
        assert_eq!(PythonPytest.generated_dirs(), GENERATED_DIRS);
        for name in GENERATED_DIRS {
            assert!(is_generated_python_dir(Path::new(name), Path::new("/nonexistent")), "{name}");
        }
    }

    /// End to end with real pytest, the reviewer's directory-changing read: the test enters
    /// `fixtures/` and reads `case.json` there. The recorder resolves the relative path at the
    /// time of the open and freezes `fixtures/case.json`; changing it would make the unchanged
    /// failing test pass and is rejected, both through the recorded input and by the allowlist.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn reads_after_a_directory_change_are_recorded_where_they_happen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("pytest.ini", "[pytest]\n");
        write("pyproject.toml", PYPROJECT);
        write("calc.py", "def add(a, b):\n    return a - b\n");
        write("fixtures/case.json", "[1, 2, 3]\n");
        write(
            FILE,
            "import json, os\nfrom calc import add\n\ndef test_issue_7_fails():\n    os.chdir(\"fixtures\")\n    try:\n        a, b, expected = json.load(open(\"case.json\"))\n    finally:\n        os.chdir(\"..\")\n    assert add(a, b) == expected, \"sum must be three\"\n",
        );
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let spec = PythonPytest.reproduction_environment(PythonPytest.regression_command(&rec(), root).unwrap(), root);
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");
        assert!(out.contains("LYRA-READ fixtures/case.json"), "{out}");
        let loaded = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap();
        assert_eq!(loaded, ["fixtures/case.json"], "{out}");

        let frozen: HashMap<String, String> = [FILE, "pyproject.toml", "calc.py", "fixtures/case.json"]
            .into_iter()
            .map(|p| (p.to_string(), std::fs::read_to_string(root.join(p)).unwrap()))
            .collect();
        let read_checkpoint = |p: &str| frozen.get(p).cloned();
        let read_now = |p: &str| std::fs::read_to_string(root.join(p)).ok();
        write("fixtures/case.json", "[4, 1, 3]\n");
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Passed, "the attack works without the lock: {out}");
        let change = vec![("M".to_string(), "fixtures/case.json".to_string())];
        let with_recorder = ReproRecord { frozen_inputs: loaded, ..rec() };
        let err = check_fix_changes_with(&change, &with_recorder, &read_checkpoint, &read_now).unwrap_err();
        assert!(err.contains("fixtures/case.json") && err.contains("repro input"), "recorded evidence: {err}");
        assert!(check_fix_changes_with(&change, &rec(), &read_checkpoint, &read_now).is_err(), "allowlist alone");
    }

    /// End to end with real pytest, the reviewer's manifest-as-input case: the regression
    /// reads the root `pyproject.toml` itself (its `[project] dependencies`) and passes the
    /// value to production code. pytest's own start-up read of the file is not an input, but
    /// this read happens during the session, so the manifest is frozen: an edit to the
    /// dependency field alone, which would make the unchanged failing test pass, is rejected
    /// even though dependency fields are otherwise editable. A test that does not read the
    /// manifest leaves its dependency fields editable.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn a_manifest_the_regression_reads_is_frozen_even_in_its_dependency_fields() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        let manifest =
            "[project]\nname = \"calc\"\ndependencies = [\"bad\"]\n[tool.pytest.ini_options]\naddopts = \"\"\n";
        write("pyproject.toml", manifest);
        write("calc.py", "def accepted(deps):\n    return deps == [\"bad\"]\n");
        write(
            FILE,
            "import tomllib\nfrom calc import accepted\n\ndef test_issue_7_fails():\n    deps = tomllib.load(open(\"pyproject.toml\", \"rb\"))[\"project\"][\"dependencies\"]\n    assert not accepted(deps), \"bad dependencies must be rejected\"\n",
        );
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let spec = PythonPytest.reproduction_environment(PythonPytest.regression_command(&rec(), root).unwrap(), root);
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");
        let loaded = PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap();
        assert_eq!(loaded, ["pyproject.toml"], "the regression's own read of the manifest: {out}");

        let frozen: HashMap<String, String> = [FILE, "pyproject.toml", "calc.py"]
            .into_iter()
            .map(|p| (p.to_string(), std::fs::read_to_string(root.join(p)).unwrap()))
            .collect();
        let read_checkpoint = |p: &str| frozen.get(p).cloned();
        let read_now = |p: &str| std::fs::read_to_string(root.join(p)).ok();
        write("pyproject.toml", &manifest.replace("[\"bad\"]", "[\"good\"]"));
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Passed, "the attack works without the lock: {out}");
        let change = vec![("M".to_string(), "pyproject.toml".to_string())];
        let with_recorder = ReproRecord { frozen_inputs: loaded, ..rec() };
        let err = check_fix_changes_with(&change, &with_recorder, &read_checkpoint, &read_now).unwrap_err();
        assert!(err.contains("pyproject.toml") && err.contains("repro input"), "{err}");

        // A regression that does not read the manifest: only pytest's start-up read happens,
        // nothing is frozen, and a dependency edit stays allowed.
        write("pyproject.toml", manifest);
        write(FILE, "from calc import accepted\n\ndef test_issue_7_fails():\n    assert not accepted([\"bad\"]), \"bad dependencies must be rejected\"\n");
        let out = proc::run(&spec, timeout, &cancel).await.unwrap().combined();
        assert_eq!(classify_pytest(&out, ID_), RegressionOutcome::Failed, "{out}");
        assert!(PythonPytest.loaded_test_inputs(&out, &rec(), root).unwrap().is_empty(), "{out}");
        let frozen: HashMap<String, String> = [FILE, "pyproject.toml", "calc.py"]
            .into_iter()
            .map(|p| (p.to_string(), std::fs::read_to_string(root.join(p)).unwrap()))
            .collect();
        let read_checkpoint = |p: &str| frozen.get(p).cloned();
        write("pyproject.toml", &manifest.replace("[\"bad\"]", "[\"bad\", \"attrs\"]"));
        check_fix_changes_with(&change, &rec(), &read_checkpoint, &read_now).unwrap();
    }

    #[test]
    fn profile_surface() {
        let p = PythonPytest;
        assert_eq!((p.id(), p.language()), (ID, "Python"));
        assert!(p.is_source_file(Path::new("src/app/core.py")));
        assert!(p.is_source_file(Path::new("pyproject.toml")));
        assert!(!p.is_source_file(Path::new("src/main.rs")));
        assert!(
            p.is_test_path("tests/unit/test_core.py")
                && p.is_test_path("pkg/core_test.py")
                && !p.is_test_path("pkg/core.py")
        );
        assert!(p.prompt_macros().contains("macro repro("));
        assert!(p.preparation(Path::new("/w"), &|_| false).is_none());
        assert!(
            first_error_line(FAILED, Some(1)).starts_with("E       AssertionError")
                || first_error_line(FAILED, Some(1)).starts_with("FAILED")
        );
    }
}
