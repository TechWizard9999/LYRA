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

const PYTEST_CONFIG_FILES: [&str; 4] = ["pytest.ini", ".pytest.ini", "pytest.toml", ".pytest.toml"];

/// Verification provenance: every project name (one per line on stdin) must be found inside
/// the checkout under test (the working directory). `find_spec` locates a top-level name
/// without importing it, so no project code runs. A name that is missing, resolves elsewhere
/// (an editable install of another clone, a regular package shadowing a namespace package) or
/// has any namespace portion outside the checkout fails, by name, with exit status 1.
const PROVENANCE_CHECK: &str = "import importlib.util, os, sys
root = os.path.realpath(os.getcwd())
bad = []
for name in [line.strip() for line in sys.stdin if line.strip()]:
    spec = importlib.util.find_spec(name)
    places = [spec.origin] if spec and spec.origin not in (None, 'built-in', 'frozen') else list(getattr(spec, 'submodule_search_locations', None) or [])
    places = [os.path.realpath(p) for p in places]
    inside = bool(places) and all(p == root or p.startswith(root + os.sep) for p in places)
    print(name, '->', ', '.join(places) or 'not found')
    if not inside:
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
    let re = RE.get_or_init(|| Regex::new(r#"(?mi)(^\s*|["'])pytest\s*([<>=!~;\[,"']|$)"#).unwrap());
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

/// The sections of `file` that configure pytest collection or plugins, in order.
fn pytest_sections(file: &str, text: &str) -> Vec<(String, Vec<String>)> {
    let name = guards::base_name(file);
    ini_sections(text)
        .into_iter()
        .filter(|(header, _)| {
            pytest_section_rules().iter().any(|(f, rule)| *f == name && rule(header))
                || (name == "pyproject.toml" && header == "project.entry-points.pytest11")
        })
        .collect()
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
        // Only names that are never source. Virtual environments and build output are
        // recognised by markers in `is_generated_dir`, so `src/env/` or `src/build/` stay visible.
        &["__pycache__", "node_modules", "site-packages"]
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
        Ok(CommandSpec::new(PYTHON, cwd).args(["-m", "pytest"]).args(PYTEST_FLAGS).arg(id))
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
                spec: CommandSpec::new(PYTHON, worktree).args(["-c", PROVENANCE_CHECK]).stdin(names.join("\n") + "\n"),
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
    if ["__pycache__", "node_modules", "site-packages"].contains(&name) || name.ends_with(".egg-info") {
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
/// ([`importable_names`]) plus each top-level name the frozen regression imports that exists
/// as project code anywhere in the checkout (for example a `lib/pkg` layout), which must then
/// also resolve inside it. Third-party and standard-library imports are not project code.
pub fn provenance_names(root: &Path, test_file: &str) -> Vec<String> {
    let mut names = importable_names(root);
    if let Ok(source) = std::fs::read_to_string(root.join(test_file)) {
        let tree = test_tree(test_file);
        for module in imported_modules(&source) {
            let Some(top) = module.split('.').next().filter(|t| !t.is_empty()) else { continue };
            if identifier_re().is_match(top) && project_code_named(root, top, &tree) {
                names.push(top.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Whether the checkout holds project code called `name` (a directory with Python files or a
/// `name.py`), outside the test tree, virtual environments and build output, up to four levels.
fn project_code_named(root: &Path, name: &str, test_tree: &str) -> bool {
    fn visit(root: &Path, dir: &Path, depth: usize, name: &str, test_tree: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else { return false };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let relative = path.strip_prefix(root).unwrap_or(&path);
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_file() && file_name == format!("{name}.py") {
                return true;
            }
            if !kind.is_dir() || file_name.starts_with('.') || is_generated_python_dir(relative, &path) {
                continue;
            }
            let rel = relative.to_string_lossy().replace('\\', "/");
            if matches!(file_name.as_str(), "tests" | "test" | "testing") || (!test_tree.is_empty() && rel == test_tree)
            {
                continue;
            }
            if file_name == name
                && std::fs::read_dir(&path)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().ends_with(".py"))
            {
                return true;
            }
            if depth < 4 && visit(root, &path, depth + 1, name, test_tree) {
                return true;
            }
        }
        false
    }
    visit(root, root, 1, name, test_tree)
}

/// The project's own top-level packages and modules in the repository root and `src/`, which
/// the verification provenance step requires to resolve inside the checkout: regular packages
/// and modules in both, and PEP 420 namespace packages (a directory of Python files without
/// `__init__.py`) under `src/`. Tests, docs, tooling scripts, virtual environments and build
/// output are not project code. There is no limit on the number of names.
pub fn importable_names(root: &Path) -> Vec<String> {
    const NOT_PROJECT_DIRS: [&str; 12] = [
        "tests",
        "test",
        "testing",
        "docs",
        "doc",
        "examples",
        "scripts",
        "benchmarks",
        "build",
        "dist",
        "site-packages",
        "node_modules",
    ];
    const NOT_PROJECT_MODULES: [&str; 7] =
        ["setup", "conftest", "noxfile", "fabfile", "tasks", "manage", "sitecustomize"];
    let mut names = Vec::new();
    for base in [root.to_path_buf(), root.join("src")] {
        let Ok(entries) = std::fs::read_dir(&base) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else { continue };
            let Ok(kind) = entry.file_type() else { continue };
            let candidate = if kind.is_dir() {
                let has_python = || {
                    std::fs::read_dir(&path)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .any(|e| e.file_name().to_string_lossy().ends_with(".py"))
                };
                let package = path.join("__init__.py").is_file() || (base != root && has_python());
                (!NOT_PROJECT_DIRS.contains(&file_name.as_str()) && !path.join("pyvenv.cfg").exists() && package)
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

/// Module names a Python source imports: `import a.b, c as d`, `from a.b import c, d`,
/// relative `from . import x` / `from ..m import y` (kept with their dots), parenthesised and
/// backslash-continued lists, imports inside functions, and dotted names in string literals
/// (`importlib.import_module("tests.test_helpers")`). For `from m import n` both `m` and `m.n`
/// are returned, because `n` may be a submodule. Over-approximation is intended: the result
/// only decides what is locked or checked.
pub fn imported_modules(source: &str) -> Vec<String> {
    static STRING: OnceLock<Regex> = OnceLock::new();
    let string_re = STRING.get_or_init(|| Regex::new(r#"["']([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)["']"#).unwrap());
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut depth = 0i64;
    for raw in source.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if current.is_empty() && !(line.starts_with("import ") || line.starts_with("from ")) {
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
        for part in statement.split(';').map(str::trim) {
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
    modules.extend(string_re.captures_iter(source).map(|c| c[1].to_string()));
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

/// Test-side files the frozen regression loads, transitively: whatever its imports (and those
/// of the files found) resolve to inside the test tree, or to a test-named module or
/// `conftest.py` anywhere. Production modules it imports are not followed: they are what the
/// fix is meant to change.
pub fn regression_imports(test_file: &str, read: FileReader<'_>) -> std::collections::BTreeSet<String> {
    let tree = test_tree(test_file);
    let exists = |p: &str| read(p).is_some();
    let test_side = |path: &str| {
        let name = guards::base_name(path);
        (!tree.is_empty() && path.strip_prefix(tree.as_str()).is_some_and(|r| r.starts_with('/')))
            || name == "conftest.py"
            || (name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py")))
    };
    let mut locked = std::collections::BTreeSet::new();
    let mut queue = vec![test_file.to_string()];
    while let Some(file) = queue.pop() {
        let Some(source) = read(&file) else { continue };
        for module in imported_modules(&source) {
            for dependency in module_files(&module, &file, &exists) {
                if dependency != test_file
                    && test_side(&dependency)
                    && locked.len() < 500
                    && locked.insert(dependency.clone())
                {
                    queue.push(dependency);
                }
            }
        }
    }
    locked
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

/// The directory tree whose infrastructure the frozen test depends on: the deepest ancestor
/// named `tests`, `test` or `testing`, otherwise the test module's own directory (`""` for the
/// repository root).
fn test_tree(test_file: &str) -> String {
    let parts: Vec<&str> = test_file.split('/').collect();
    let dirs = &parts[..parts.len().saturating_sub(1)];
    match dirs.iter().rposition(|d| matches!(*d, "tests" | "test" | "testing")) {
        Some(i) => dirs[..=i].join("/"),
        None => dirs.join("/"),
    }
}

/// Inside the test tree everything except other test modules is locked: a package
/// initializer runs on import, a helper or a new module can change or shadow what the
/// unchanged test exercises, and data files are its inputs. Other test modules may still be
/// corrected when they encoded the buggy behaviour. A test at the repository root locks only
/// the root `__init__.py`, because root modules are production code.
fn locked_by_test_tree(path: &str, tree: &str) -> bool {
    let name = guards::base_name(path);
    if tree.is_empty() {
        return path == "__init__.py";
    }
    let inside = path.strip_prefix(tree).is_some_and(|rest| rest.starts_with('/'));
    let test_module = name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"));
    inside && !test_module
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
    // 1. The test module is byte-for-byte its frozen form.
    let mut violations = guards::frozen_test_violations(changes, rec, read_checkpoint, read_now);

    // 2. Nothing that changes collection, imports or plugin loading, and none of the test
    //    infrastructure next to the frozen test.
    let test_file = guards::norm_path(&rec.test_file);
    let tree = test_tree(&test_file);
    let imported = regression_imports(&test_file, read_checkpoint);
    for (status, raw) in changes {
        let path = guards::norm_path(raw);
        if let Some(reason) = collection_control_violation(&path) {
            violations.push(format!("{status} {path}: {reason}"));
            continue;
        }
        if imported.contains(&path) {
            violations.push(format!("{status} {path}: imported by the frozen regression, so it is locked with it"));
            continue;
        }
        if path != test_file && locked_by_test_tree(&path, &tree) {
            let tree = if tree.is_empty() { "the repository root" } else { tree.as_str() };
            violations.push(format!(
                "{status} {path}: test infrastructure in {tree} (package initializers, helper modules, data) is locked with the frozen test"
            ));
            continue;
        }
        let name = guards::base_name(&path);
        if ["pyproject.toml", "setup.cfg", "tox.ini"].contains(&name) {
            let old = pytest_sections(&path, &read_checkpoint(&path).unwrap_or_default());
            let new = pytest_sections(&path, &read_now(&path).unwrap_or_default());
            if old != new {
                violations.push(format!("{status} {path}: its pytest configuration or pytest11 plugins changed"));
            }
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
    const NAMES: [&str; 8] = [
        "pytest.ini",
        ".pytest.ini",
        "pytest.toml",
        ".pytest.toml",
        "conftest.py",
        "pyproject.toml",
        "setup.cfg",
        "tox.ini",
    ];
    for ancestor in checkout.parent().into_iter().flat_map(Path::ancestors) {
        for name in NAMES {
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
        std::fs::write(dir.path().join("requirements-dev.txt"), "pytest-cov-but-not-really-pytestish\n").unwrap();
        assert!(detect(dir.path()).is_none());
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
        let pyproject = "[project]\nname = \"x\"\n[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n";
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
        let frozen = HashMap::from([(FILE, "from tests.helpers import expected\n")]);
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
            assert!(err.contains("test infrastructure in tests"), "{path}: {err}");
        }
        assert!(check("tests/test_other.py", "M").is_ok(), "other tests may be corrected");
        assert!(check("textkit/__init__.py", "M").is_ok(), "production package initializers may change");
        assert_eq!(test_tree("src/pkg/tests/unit/test_x.py"), "src/pkg/tests");
        assert_eq!(test_tree("pkg/test_x.py"), "pkg");
        assert_eq!(test_tree("test_x.py"), "");
        assert!(locked_by_test_tree("__init__.py", "") && !locked_by_test_tree("calc.py", ""));
        assert!(!locked_by_test_tree("tests_extra/helpers.py", "tests"));
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
            assert!(err.contains(path) && err.contains("locked"), "{err}");
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
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        assert_eq!(importable_names(root), ["calc", "pkg", "toplevel"]);
        let steps = PythonPytest.verification_steps(&VerifyCommands::default(), &rec(), root).unwrap();
        assert_eq!(steps.iter().map(|s| s.name).collect::<Vec<_>>(), ["imports", "repro", "tests"]);
        assert_eq!(steps[0].spec.args.len(), 2, "names travel on stdin, not argv");
        assert_eq!(steps[0].spec.stdin.as_deref(), Some("calc\npkg\ntoplevel\n"));
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
        ]);
        let read = reader(&files);
        let locked = regression_imports(FILE, &read);
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
            assert!(err.contains("imported by the frozen regression"), "{path}: {err}");
        }
        assert!(check("tests/test_other.py").is_ok(), "tests the regression does not import stay editable");

        // pytest's default import mode puts the first directory without __init__.py on sys.path,
        // so a bare `import test_helpers` from tests/ resolves to tests/test_helpers.py.
        let bare =
            HashMap::from([(FILE, "from test_helpers import EXPECTED\n"), ("tests/test_helpers.py", "EXPECTED = 3\n")]);
        assert!(regression_imports(FILE, &reader(&bare)).contains("tests/test_helpers.py"));
        // A helper named like a test outside the test tree is locked too.
        let outside = HashMap::from([
            (FILE, "from support.test_fixtures import DATA\n"),
            ("support/test_fixtures.py", "DATA = 1\n"),
        ]);
        assert!(regression_imports(FILE, &reader(&outside)).contains("support/test_fixtures.py"));
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
            "project code the regression imports, outside root/src: {names:?}"
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
    /// candidate's PEP 420 namespace package; the provenance step must catch it by name.
    #[tokio::test]
    #[ignore = "needs a provisioned python3 with pytest on PATH"]
    async fn a_regular_package_elsewhere_cannot_hide_behind_a_namespace_package() {
        let dir = tempfile::tempdir().unwrap();
        let timeout = Duration::from_secs(120);
        let cancel = CancellationToken::new();
        let write = |path: std::path::PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        let checkout = dir.path().join("checkout");
        write(checkout.join("src/pkg/module.py"), "def answer():\n    return 41\n");
        write(checkout.join("pytest.ini"), "[pytest]\n");
        write(
            checkout.join(FILE),
            "from pkg.module import answer\n\ndef test_issue_7_fails():\n    assert answer() == 42, \"sum must be three\"\n",
        );
        let external = dir.path().join("site");
        write(external.join("pkg/__init__.py"), "");
        write(external.join("pkg/module.py"), "def answer():\n    return 42\n");

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
        // ...so verification fails visibly, naming the package.
        let steps = PythonPytest.verification_steps(&VerifyCommands::default(), &rec(), &checkout).unwrap();
        assert_eq!(steps[0].name, "imports");
        let env_root = dir.path().join("env");
        let check =
            proc::run(&with_venv(PythonPytest.command_environment(steps[0].spec.clone(), &env_root)), timeout, &cancel)
                .await
                .unwrap();
        assert!(
            !check.success() && check.stdout.contains("ERROR: pkg does not resolve inside the checkout"),
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
