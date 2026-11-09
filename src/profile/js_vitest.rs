//! The `js-vitest` execution profile (PLAN §13.8): JavaScript and TypeScript repositories
//! tested with Vitest.
//!
//! - **Dependencies** come from the operator's own install. LYRA installs nothing: the
//!   repository's `node_modules` is linked one level above each checkout (Node and Vite
//!   resolve packages by walking up parent directories), so nothing appears inside the
//!   checkout and the guards see no extra file. Workspaces (monorepos) are Phase 3b.
//! - **Identity**: the test file plus the test's full name, its `describe` chain and test
//!   name joined by ` > ` (`add > sums two numbers`).
//! - **Evidence**: a fixed recorder runs Vitest in-process through its Node API and reports
//!   every test's full name, state and errors as JSON, plus every module the run loaded; a
//!   module loaded from the operator's clone instead of the checkout fails the run.
//! - **Lock**: an allowlist of declared production code, as for Python: tests, setup files,
//!   snapshots, fixtures, every configuration file, every `package.json`, lockfiles and
//!   anything under `node_modules` are frozen.

use super::{Discovery, ExecutionProfile, FileReader, Preparation, RegressionOutcome, ResolvedProfile, VerifyStep};
use crate::config::VerifyCommands;
use crate::guards::{self, ReproRecord};
use crate::proc::{self, CommandOutput, CommandSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const ID: &str = "js-vitest";

const NODE: &str = "node";

const DETECTION: &str = "a root package.json with Vitest (a vitest/vite config file, or vitest in its dependencies)";

/// Source extensions Vitest runs; `.d.ts` files are covered by `ts`.
const SOURCE_EXTENSIONS: [&str; 8] = ["js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx"];

/// Configuration file stems; each with any of [`SOURCE_EXTENSIONS`] is a config file.
const CONFIG_STEMS: [&str; 6] =
    ["vitest.config", "vite.config", "vitest.workspace", "vitest.projects", "babel.config", "vitest.setup"];

/// Directory names that are never source.
const GENERATED_DIRS: [&str; 5] = ["node_modules", "coverage", ".vite", ".turbo", ".vitest"];

/// Build output named by `main` / `module` / `exports`: never production code to edit.
const BUILD_OUTPUT: [&str; 4] = ["dist", "build", "out", "coverage"];

/// Directory names holding test infrastructure, anywhere.
const TEST_DIRS: [&str; 9] =
    ["test", "tests", "__tests__", "__mocks__", "__snapshots__", "__fixtures__", "fixtures", "testdata", "test_data"];

/// The recorder: runs Vitest in-process and reports, after the run, a JSON summary of every
/// test (its full name below the file, mode, state and error messages), file-level errors
/// and unhandled errors, then every module the run loaded from inside the checkout
/// (`LYRA-LOADED`) and every one loaded from the operator's clone outside its `node_modules`
/// (`LYRA-FOREIGN`), then `LYRA-END`. Arguments: test file filter, name pattern (both may be
/// empty). `LYRA_VITEST_CACHE` moves Vite's cache out of the shared `node_modules`;
/// `LYRA_VITEST_MODE=discover` exits 0 once the report is printed (an empty run is evidence).
/// The exit status is otherwise Vitest's.
const RECORDER: &str = r#"import { startVitest } from 'vitest/node'
import fs from 'node:fs'
import path from 'node:path'
const [file, pattern] = process.argv.slice(1)
const root = fs.realpathSync(process.cwd())
let operator = null
try { operator = path.dirname(fs.realpathSync(path.join(root, '..', 'node_modules'))) } catch {}
const rel = p => path.relative(root, p).split(path.sep).join('/')
const inside = (p, dir) => p === dir || p.startsWith(dir + path.sep)
let vitest
try {
  vitest = await startVitest('test', file ? [file] : [], {
    run: true, watch: false, color: false, passWithNoTests: false,
    testNamePattern: pattern || undefined,
    reporters: ['default'],
  }, { cacheDir: process.env.LYRA_VITEST_CACHE || undefined, logLevel: 'error' })
} catch (error) {
  console.error('LYRA-STARTUP-ERROR ' + String(error && error.stack || error))
}
const files = vitest ? vitest.state.getFiles() : []
const tests = []
const walk = (task, names) => {
  if (task.type === 'test') {
    tests.push({ file: rel(task.file ? task.file.filepath : ''), name: [...names, task.name].join(' > '), mode: task.mode, state: task.result ? task.result.state : null, errors: (task.result && task.result.errors || []).map(e => String(e && e.message || e)) })
    return
  }
  for (const child of task.tasks || []) walk(child, task.type === 'suite' && task.file !== task ? [...names, task.name] : names)
}
for (const f of files) walk(f, [])
const fileErrors = files.filter(f => (f.result && f.result.errors || []).length).map(f => ({ file: rel(f.filepath), errors: f.result.errors.map(e => String(e && e.message || e)) }))
const unhandled = vitest && vitest.state.getUnhandledErrors ? vitest.state.getUnhandledErrors().length : 0
console.log('LYRA-RESULT ' + JSON.stringify({ tests, fileErrors, unhandled, started: !!vitest }))
const loaded = new Set()
for (const project of vitest ? vitest.projects : []) {
  const graph = project.vite && project.vite.moduleGraph
  for (const id of graph ? graph.idToModuleMap.keys() : []) loaded.add(id)
}
for (const id of loaded) {
  if (typeof id !== 'string' || id.startsWith('\0')) continue
  let real = id.split('?')[0]
  if (!path.isAbsolute(real)) continue
  try { real = fs.realpathSync(real) } catch { continue }
  if (inside(real, root)) console.log('LYRA-LOADED ' + rel(real))
  else if (operator && inside(real, operator) && !inside(real, path.join(operator, 'node_modules'))) console.log('LYRA-FOREIGN ' + real)
}
console.log('LYRA-END')
if (vitest) await vitest.close()
if (process.env.LYRA_VITEST_MODE === 'discover' && vitest) process.exit(0)
process.exit(vitest ? (process.exitCode ?? 0) : 2)"#;
const RESULT_PREFIX: &str = "LYRA-RESULT ";
const LOADED_PREFIX: &str = "LYRA-LOADED ";
const FOREIGN_PREFIX: &str = "LYRA-FOREIGN ";
const END_MARKER: &str = "LYRA-END";

pub struct JsVitest;

// ---------------------------------------------------------------------------------------
// Detection and layout
// ---------------------------------------------------------------------------------------

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn has_config(dir: &Path, stems: &[&str]) -> Option<String> {
    for stem in stems {
        for ext in SOURCE_EXTENSIONS {
            let name = format!("{stem}.{ext}");
            if dir.join(&name).is_file() {
                return Some(name);
            }
        }
    }
    None
}

fn declares_vitest(manifest: &Value) -> bool {
    ["devDependencies", "dependencies", "peerDependencies", "optionalDependencies"]
        .iter()
        .any(|key| manifest.get(key).and_then(|d| d.get("vitest")).is_some())
}

/// Vitest evidence at the worktree root. Files are read, nothing is executed.
pub fn detect(worktree: &Path) -> Option<ResolvedProfile> {
    let manifest = read_json(&worktree.join("package.json"))?;
    let mut evidence = vec!["package.json".to_string()];
    if let Some(config) = has_config(worktree, &["vitest.config", "vitest.workspace"]) {
        evidence.push(config);
    } else if declares_vitest(&manifest) {
        if let Some(config) = has_config(worktree, &["vite.config"]) {
            evidence.push(config);
        }
    } else {
        return None;
    }
    Some(ResolvedProfile { id: ID.into(), version: super::CONTRACT_VERSION, project_dir: String::new(), evidence })
}

/// Workspace (monorepo) declarations: npm/yarn `workspaces`, `pnpm-workspace.yaml`. One
/// package at a time is Phase 3b.
fn workspaces(worktree: &Path) -> Option<&'static str> {
    if worktree.join("pnpm-workspace.yaml").is_file() {
        return Some("pnpm-workspace.yaml");
    }
    read_json(&worktree.join("package.json"))
        .filter(|m| m.get("workspaces").is_some())
        .map(|_| "package.json workspaces")
}

fn extension(path: &str) -> Option<&str> {
    let name = guards::base_name(path);
    name.rsplit_once('.').map(|(_, ext)| ext)
}

fn is_source_code(path: &str) -> bool {
    extension(path).is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
}

/// A test-named module: `*.test.*` / `*.spec.*` with a source extension.
fn is_test_module(path: &str) -> bool {
    let name = guards::base_name(path);
    is_source_code(path) && (name.contains(".test.") || name.contains(".spec."))
}

/// A setup file by name: Vitest `setupFiles` / `globalSetup` conventions (`setupTests.ts`,
/// `vitest.setup.ts`, `test-setup.js`, `global-setup.ts`).
fn is_setup_file(path: &str) -> bool {
    let name = guards::base_name(path).to_ascii_lowercase();
    let stem = name.split('.').next().unwrap_or_default();
    is_source_code(path)
        && (matches!(
            stem,
            "setuptests"
                | "setup-tests"
                | "setup_tests"
                | "testsetup"
                | "test-setup"
                | "globalsetup"
                | "global-setup"
                | "vitest-setup"
        ) || name.starts_with("vitest.setup.")
            || name.starts_with("test.setup."))
}

/// Test infrastructure anywhere: test modules, setup files, snapshots, and every file below a
/// test, mock, snapshot or fixture directory, also inside declared production code.
fn is_test_infrastructure(path: &str) -> bool {
    is_test_module(path)
        || is_setup_file(path)
        || guards::base_name(path).ends_with(".snap")
        || path.split('/').rev().skip(1).any(|dir| TEST_DIRS.contains(&dir))
}

pub fn is_test_path(path: &str) -> bool {
    is_test_infrastructure(path)
}

/// Configuration and manifests a fix may never change, wherever they are: every
/// `package.json`, lockfiles, package-manager settings, TypeScript/Babel/SWC settings and
/// every Vitest or Vite configuration; at the root also any `*.config.*` file.
fn configuration_violation(path: &str) -> Option<&'static str> {
    let name = guards::base_name(path);
    let root = !path.contains('/');
    if path.split('/').any(|c| c == "node_modules") {
        return Some("dependencies cannot be added or changed inside the checkout");
    }
    if matches!(
        name,
        "package.json"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "bun.lockb"
            | "bun.lock"
    ) {
        return Some("package manifests and lockfiles are frozen: LYRA installs nothing, so a dependency change cannot be verified");
    }
    if matches!(
        name,
        ".npmrc"
            | ".yarnrc"
            | ".yarnrc.yml"
            | ".pnpmfile.cjs"
            | "pnpm-workspace.yaml"
            | ".nvmrc"
            | ".node-version"
            | ".swcrc"
            | "jsconfig.json"
    ) || name.starts_with(".babelrc")
        || (name.starts_with("tsconfig") && name.ends_with(".json"))
    {
        return Some("package-manager, TypeScript and transpiler settings are frozen");
    }
    if CONFIG_STEMS.iter().any(|stem| name.starts_with(&format!("{stem}.")) && is_source_code(name))
        || (root && name.contains(".config.") && is_source_code(name))
    {
        return Some("test-runner and build configuration is frozen");
    }
    None
}

// ---------------------------------------------------------------------------------------
// Production code
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Classification {
    Production,
    TestSupport,
    Unknown,
}

/// What the repository declares as its production code, as top-level paths (directories or
/// root files) that exist in the frozen checkout: the targets of `package.json` `main`,
/// `module`, `browser`, `types`, `typings`, `bin` and `exports` and its `files` entries, the
/// `rootDir` and `include` entries of `tsconfig.json`, and `sources = [...]` under
/// `[profiles.js-vitest]` in the repository's `lyra.toml`. Build output named by the package
/// entry points (`dist/`, `build/`, `out/`) is not code to edit; wildcard-only entries declare
/// nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProductionCode {
    roots: BTreeSet<String>,
    missing: BTreeSet<String>,
}

fn top_component(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_start_matches("./").trim_start_matches('/');
    let top = raw.split('/').next()?.trim();
    let literal: String = top.chars().take_while(|c| !matches!(c, '*' | '?' | '{' | '[' | '!')).collect();
    (!literal.is_empty() && literal == top && top != "." && top != "..").then(|| top.to_string())
}

fn strings_in(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| strings_in(v, out)),
        Value::Object(map) => map.values().for_each(|v| strings_in(v, out)),
        _ => {}
    }
}

/// Removes `//` and `/* */` comments and trailing commas from JSONC (tsconfig) outside strings.
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut previous = ' ';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
            }
            _ => out.push(c),
        }
    }
    let trailing = regex::Regex::new(r",(\s*[}\]])").unwrap();
    trailing.replace_all(&out, "$1").into_owned()
}

impl ProductionCode {
    pub fn declared(read: FileReader<'_>) -> Self {
        let mut code = Self::default();
        let mut entry_points = Vec::new();
        let mut explicit = Vec::new();
        if let Some(manifest) = read("package.json").and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            for key in ["main", "module", "browser", "types", "typings", "bin", "exports"] {
                if let Some(value) = manifest.get(key) {
                    strings_in(value, &mut entry_points);
                }
            }
            if let Some(files) = manifest.get("files") {
                strings_in(files, &mut explicit);
            }
        }
        if let Some(tsconfig) = read("tsconfig.json").and_then(|t| serde_json::from_str::<Value>(&strip_jsonc(&t)).ok())
        {
            if let Some(root_dir) = tsconfig.get("compilerOptions").and_then(|o| o.get("rootDir")) {
                strings_in(root_dir, &mut explicit);
            }
            if let Some(include) = tsconfig.get("include") {
                strings_in(include, &mut explicit);
            }
        }
        if let Some(doc) = read("lyra.toml").and_then(|t| t.parse::<toml::Value>().ok()) {
            if let Some(sources) = doc.get("profiles").and_then(|p| p.get(ID)).and_then(|p| p.get("sources")) {
                for s in sources.as_array().into_iter().flatten().filter_map(toml::Value::as_str) {
                    explicit.push(s.to_string());
                }
            }
        }
        for raw in entry_points {
            if let Some(top) = top_component(&raw).filter(|t| !BUILD_OUTPUT.contains(&t.as_str())) {
                code.add(&top, read);
            }
        }
        for raw in explicit {
            if let Some(top) = top_component(&raw) {
                code.add(&top, read);
            }
        }
        code
    }

    fn add(&mut self, top: &str, read: FileReader<'_>) {
        if TEST_DIRS.contains(&top) || GENERATED_DIRS.contains(&top) || configuration_violation(top).is_some() {
            return;
        }
        if read(top).is_some() || read(&format!("{top}/")).is_some() {
            self.roots.insert(top.to_string());
        } else {
            self.missing.insert(top.to_string());
        }
    }

    fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    fn declares(&self, path: &str) -> bool {
        self.roots.contains(path) || path.split_once('/').is_some_and(|(top, _)| self.roots.contains(top))
    }

    fn classify(&self, path: &str) -> Classification {
        if is_test_infrastructure(path) {
            return Classification::TestSupport;
        }
        if self.is_empty() {
            return if is_source_code(path) { Classification::Unknown } else { Classification::TestSupport };
        }
        if self.declares(path) {
            Classification::Production
        } else {
            Classification::TestSupport
        }
    }

    fn describe(&self) -> String {
        self.roots.iter().map(|r| format!("`{r}`")).collect::<Vec<_>>().join(", ")
    }

    fn unknown_message(&self, paths: &BTreeSet<String>) -> String {
        let missing = if self.missing.is_empty() {
            String::new()
        } else {
            format!(
                " (declared but not present in the checkout, so ignored: {})",
                self.missing.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ")
            )
        };
        format!(
            "the frozen regression loads {}, which LYRA cannot tell apart from production code because the repository \
             declares no source directories; declare them (`files`, `main`, `exports` in package.json, `include` or \
             `rootDir` in tsconfig.json, or `sources = [...]` under `[profiles.{ID}]` in the repository's lyra.toml): \
             everything the regression loads outside them is then frozen as test support{missing}",
            paths.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ")
        )
    }
}

/// Reads files below `root`; `dir/` asks whether a directory exists (`Some("")`).
fn worktree_reader(root: &Path) -> impl Fn(&str) -> Option<String> + '_ {
    move |p: &str| {
        if let Some(dir) = p.strip_suffix('/') {
            return root.join(dir).is_dir().then(String::new);
        }
        let path = root.join(p);
        if path.is_dir() {
            return Some(String::new());
        }
        std::fs::read_to_string(path).ok()
    }
}

// ---------------------------------------------------------------------------------------
// Test identity and commands
// ---------------------------------------------------------------------------------------

/// The ` > `-separated segments of a test's full name, validated.
fn name_segments(test_name: &str) -> Result<Vec<String>, String> {
    let name = test_name.trim();
    if name.is_empty() || name.len() > 500 {
        return Err("test_name must be the test's full name, `describe > test`, at most 500 characters".into());
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("test_name must not contain control characters".into());
    }
    let segments: Vec<String> = name.split(" > ").map(|s| s.trim().to_string()).collect();
    if segments.iter().any(String::is_empty) {
        return Err(format!("test_name `{name}` has an empty segment; use `describe > test`"));
    }
    Ok(segments)
}

fn validate_test_file(test_file: &str) -> Result<String, String> {
    let raw = test_file.trim();
    if raw.starts_with(['/', '\\']) || raw.chars().nth(1) == Some(':') {
        return Err(format!("test_file `{test_file}` must be a repository-relative path"));
    }
    let path = guards::norm_path(raw);
    if path.is_empty() || path.split('/').any(|c| c == ".." || c.is_empty() || c == "node_modules") {
        return Err(format!("test_file `{test_file}` must be a repository-relative path"));
    }
    if !is_test_module(&path) {
        return Err(format!("test_file `{path}` must be a Vitest test module named `*.test.<ext>` or `*.spec.<ext>`"));
    }
    Ok(path)
}

/// Vitest's `-t` pattern for the full name: suite and test names are joined by a space; the
/// pattern is anchored and every segment escaped, so only this exact name matches.
fn name_pattern(segments: &[String]) -> String {
    format!("^{}$", segments.iter().map(|s| regex::escape(s)).collect::<Vec<_>>().join(" "))
}

/// The one-line, copyable command shown in `repro.json` and the pull request.
fn regression_line(test_file: &str, segments: &[String]) -> String {
    format!("npx vitest run {test_file} -t \"{}\"", name_pattern(segments).replace('"', "\\\""))
}

fn recorder_spec(cwd: &Path, file: &str, pattern: &str) -> CommandSpec {
    CommandSpec::new(NODE, cwd).args(["--input-type=module", "-e", RECORDER]).arg(file).arg(pattern)
}

// ---------------------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct TestResult {
    file: String,
    name: String,
    mode: String,
    state: Option<String>,
    errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Report {
    tests: Vec<TestResult>,
    file_errors: Vec<(String, Vec<String>)>,
    unhandled: u64,
    started: bool,
}

fn parse_report(output: &str) -> Option<Report> {
    if !output.lines().any(|l| l.trim() == END_MARKER) {
        return None;
    }
    let line = output.lines().rev().find_map(|l| l.trim().strip_prefix(RESULT_PREFIX))?;
    let value: Value = serde_json::from_str(line).ok()?;
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let strings = |v: Option<&Value>| -> Vec<String> {
        v.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(String::from).collect()
    };
    let tests = value
        .get("tests")
        .and_then(Value::as_array)?
        .iter()
        .map(|t| TestResult {
            file: text(t, "file"),
            name: text(t, "name"),
            mode: text(t, "mode"),
            state: t.get("state").and_then(Value::as_str).map(String::from),
            errors: strings(t.get("errors")),
        })
        .collect();
    let file_errors = value
        .get("fileErrors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|f| (text(f, "file"), strings(f.get("errors"))))
        .collect();
    Some(Report {
        tests,
        file_errors,
        unhandled: value.get("unhandled").and_then(Value::as_u64).unwrap_or(0),
        started: value.get("started").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn executed(t: &TestResult) -> bool {
    matches!(t.state.as_deref(), Some("pass") | Some("fail"))
}

fn identity(rec: &ReproRecord) -> Option<(String, String)> {
    let file = validate_test_file(&rec.test_file).ok()?;
    let name = name_segments(&rec.test_name).ok()?.join(" > ");
    Some((file, name))
}

pub fn classify_report(output: &str, rec: &ReproRecord) -> RegressionOutcome {
    let Some(report) = parse_report(output) else {
        return RegressionOutcome::Other("the Vitest recorder did not report a complete result".into());
    };
    let Some((file, name)) = identity(rec) else {
        return RegressionOutcome::Other("the reproduction record does not name a valid test".into());
    };
    if !report.started {
        return RegressionOutcome::SetupFailed;
    }
    if report.unhandled > 0 || report.file_errors.iter().any(|(f, _)| *f == file) {
        return RegressionOutcome::SetupFailed;
    }
    let Some(test) = report.tests.iter().find(|t| t.file == file && t.name == name) else {
        return RegressionOutcome::NotDiscovered;
    };
    match test.state.as_deref() {
        Some("fail") => RegressionOutcome::Failed,
        Some("pass") => RegressionOutcome::Passed,
        _ => RegressionOutcome::Skipped,
    }
}

fn describe_run(outcome: &RegressionOutcome, rec: &ReproRecord) -> String {
    let name = format!("{} > {}", guards::norm_path(&rec.test_file), rec.test_name.trim());
    match outcome {
        RegressionOutcome::Failed => format!("test {name} fails"),
        RegressionOutcome::Passed => format!("test {name} passes"),
        RegressionOutcome::Skipped => format!("test {name} was skipped or marked todo"),
        RegressionOutcome::NotDiscovered => format!("test {name} was not found by Vitest"),
        RegressionOutcome::SetupFailed => format!("the test file for {name} failed to load or set up"),
        RegressionOutcome::Other(reason) => format!("test {name}: {reason}"),
    }
}

fn exactly_ours(report: &Report, file: &str, name: &str) -> Result<(), String> {
    let ran: Vec<&TestResult> = report.tests.iter().filter(|t| executed(t)).collect();
    if ran.len() != 1 || ran[0].file != file || ran[0].name != name {
        return Err(format!(
            "exactly one test must execute, `{file} > {name}`; {} ran ({})",
            ran.len(),
            ran.iter().map(|t| format!("`{} > {}`", t.file, t.name)).collect::<Vec<_>>().join(", ")
        ));
    }
    if report.unhandled > 0 || !report.file_errors.is_empty() {
        return Err("the run reported file-level or unhandled errors".into());
    }
    Ok(())
}

pub fn confirm_failure_in(run: &CommandOutput, rec: &ReproRecord) -> Result<(), String> {
    let output = run.combined();
    let (file, name) = identity(rec).ok_or("the reproduction record does not name a valid test")?;
    let report = parse_report(&output).ok_or("the Vitest recorder did not report a complete result")?;
    exactly_ours(&report, &file, &name)?;
    if report.tests.iter().find(|t| t.file == file && t.name == name).and_then(|t| t.state.as_deref()) != Some("fail") {
        return Err("the selected test did not fail".into());
    }
    if run.exit_code != Some(1) {
        return Err("reproduction must exit with Vitest's test-failure status (1)".into());
    }
    Ok(())
}

pub fn assert_passed_in(output: &str, rec: &ReproRecord) -> Result<(), String> {
    let (file, name) = identity(rec).ok_or("the reproduction record does not name a valid test")?;
    let report = parse_report(output).ok_or("the Vitest recorder did not report a complete result")?;
    exactly_ours(&report, &file, &name)?;
    match report.tests.iter().find(|t| t.file == file && t.name == name).and_then(|t| t.state.as_deref()) {
        Some("pass") => Ok(()),
        other => Err(format!("the selected test did not pass ({})", other.unwrap_or("not run"))),
    }
}

/// With the test file emptied, the recorder's report must be complete and list no test from
/// that file: the selected test cannot come from another file or a setup hook.
pub fn assert_absent(output: &str, rec: &ReproRecord) -> Result<(), String> {
    let (file, _) = identity(rec).ok_or("the reproduction record does not name a valid test")?;
    let report = parse_report(output).ok_or("discovery did not report a complete result")?;
    if !report.started {
        return Err("Vitest did not start for discovery".into());
    }
    let found: Vec<&TestResult> = report.tests.iter().filter(|t| t.file == file).collect();
    if found.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "with {file} emptied, Vitest still found {} test(s) in it: the selected test is not bound to this source",
            found.len()
        ))
    }
}

/// The modules the failing run loaded that are not production code, to freeze with the test;
/// a module loaded from the operator's clone outside its `node_modules` is an error (the
/// regression did not run the candidate's code), and so is a module the repository's
/// declarations cannot classify.
pub fn loaded_inputs(output: &str, test_file: &str, production: &ProductionCode) -> Result<Vec<String>, String> {
    if !output.lines().any(|l| l.trim() == END_MARKER) {
        return Err("the regression run did not report the modules it loaded, so LYRA cannot establish which test \
                    inputs to lock"
            .into());
    }
    let foreign: Vec<&str> = output.lines().filter_map(|l| l.trim().strip_prefix(FOREIGN_PREFIX)).collect();
    if !foreign.is_empty() {
        return Err(format!(
            "the regression loaded code from the repository LYRA was started on instead of the checkout under test \
             ({}); a workspace or a link in node_modules points back into the repository",
            foreign.iter().map(|p| format!("`{}`", p.trim())).collect::<Vec<_>>().join(", ")
        ));
    }
    let mut loaded = BTreeSet::new();
    let mut unknown = BTreeSet::new();
    for path in output.lines().filter_map(|l| l.trim().strip_prefix(LOADED_PREFIX)).map(guards::norm_path) {
        if path.is_empty() || path == test_file || path.split('/').any(|c| c == ".." || c == "node_modules") {
            continue;
        }
        match production.classify(&path) {
            Classification::Production => {}
            Classification::TestSupport => {
                loaded.insert(path);
            }
            Classification::Unknown => {
                unknown.insert(path);
            }
        }
    }
    if !unknown.is_empty() {
        return Err(production.unknown_message(&unknown));
    }
    Ok(loaded.into_iter().collect())
}

// ---------------------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------------------

/// S2 allowed set: the new test module and new non-code fixture files in a test or fixture
/// directory. Nothing else: no setup files, helpers, configuration or dependency changes.
pub fn check_repro_changes_with(changes: &[(String, String)], rec: &ReproRecord) -> Result<(), String> {
    let test_file = guards::norm_path(&rec.test_file);
    let mut violations = Vec::new();
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
        if let Some(reason) = configuration_violation(&path) {
            violations.push(format!("{status} {path}: {reason}"));
            continue;
        }
        let in_test_dir = path.split('/').rev().skip(1).any(|d| TEST_DIRS.contains(&d) && d != "__snapshots__");
        if guards::is_new(status)
            && in_test_dir
            && !is_source_code(&path)
            && !guards::base_name(&path).ends_with(".snap")
        {
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

/// S4/S5 lock, an allowlist: a fix may change only source code inside the declared
/// production roots; the frozen test and its recorded inputs take precedence, and test
/// infrastructure, configuration, manifests and anything under `node_modules` are frozen
/// wherever they are.
pub fn check_fix_changes_with(
    changes: &[(String, String)],
    rec: &ReproRecord,
    read_checkpoint: FileReader<'_>,
    read_now: FileReader<'_>,
) -> Result<(), String> {
    if changes.is_empty() {
        return Err("fix produced no changes".to_string());
    }
    let mut violations = guards::frozen_test_violations(changes, rec, read_checkpoint, read_now);
    let frozen: BTreeSet<String> =
        std::iter::once(&rec.test_file).chain(&rec.frozen_inputs).map(|p| guards::norm_path(p)).collect();
    let production = ProductionCode::declared(read_checkpoint);
    for (status, raw) in changes {
        let path = guards::norm_path(raw);
        if frozen.contains(&path) {
            continue;
        }
        if let Some(reason) = fix_may_not_change(&path, &production) {
            violations.push(format!("{status} {path}: {reason}"));
        }
    }
    if let Some((file, name)) = identity(rec) {
        let segments: Vec<String> = name.split(" > ").map(String::from).collect();
        let expected = regression_line(&file, &segments);
        if rec.command.trim() != expected {
            violations.push(format!("recorded test command changed: {:?} (expected {expected:?})", rec.command.trim()));
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!("fix violates the test lock: {}", violations.join("; ")))
    }
}

fn fix_may_not_change(path: &str, production: &ProductionCode) -> Option<String> {
    if let Some(reason) = configuration_violation(path) {
        return Some(reason.into());
    }
    if is_test_infrastructure(path) {
        return Some(
            "tests, setup files, mocks, snapshots and fixtures are frozen with the regression wherever they live"
                .into(),
        );
    }
    match production.classify(path) {
        Classification::Production if !is_source_code(path) => Some(
            "only source code may change inside production code: other files there may be test inputs, so they are \
             frozen"
                .into(),
        ),
        Classification::Production => None,
        Classification::Unknown => Some(production.unknown_message(&BTreeSet::from([path.to_string()]))),
        Classification::TestSupport => Some(format!(
            "not declared production code; a fix may change only source code in {}",
            if production.is_empty() { "the declared source directories".to_string() } else { production.describe() }
        )),
    }
}

// ---------------------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------------------

/// Scrubbed environment: no operator `NODE_OPTIONS` or `NODE_PATH`, non-interactive and
/// colourless output, Vite's cache under the run's own directory.
fn controlled_environment(spec: CommandSpec, caches: &Path) -> CommandSpec {
    const OWNED: [&str; 6] = ["NODE_OPTIONS", "NODE_PATH", "CI", "NO_COLOR", "FORCE_COLOR", "LYRA_VITEST_CACHE"];
    let mut spec = spec.scrubbed();
    spec.env.retain(|(key, _)| !OWNED.iter().any(|owned| key.eq_ignore_ascii_case(owned)));
    spec.env.push(("CI".into(), "1".into()));
    spec.env.push(("NO_COLOR".into(), "1".into()));
    spec.env.push(("FORCE_COLOR".into(), "0".into()));
    spec.env.push(("LYRA_VITEST_CACHE".into(), caches.join("vite").to_string_lossy().into_owned()));
    spec
}

/// Links `source_repo/node_modules` at `parent/node_modules`, so a checkout below `parent`
/// resolves the operator's installed packages without anything inside it changing.
pub fn link_node_modules(source_repo: &Path, parent: &Path) -> Result<(), String> {
    if let Some(kind) = workspaces(source_repo) {
        return Err(format!(
            "this repository is a JavaScript workspace ({kind}); LYRA supports one package at a time and \
             workspaces are not supported yet"
        ));
    }
    let target = source_repo.join("node_modules");
    if !target.join("vitest").is_dir() {
        return Err(format!(
            "{} has no installed Vitest; install the repository's dependencies first (`npm ci`, \
             `pnpm install --frozen-lockfile` or `yarn install --frozen-lockfile`) and retry",
            target.display()
        ));
    }
    let link = parent.join("node_modules");
    if let (Ok(existing), Ok(wanted)) = (dunce::canonicalize(&link), dunce::canonicalize(&target)) {
        if existing == wanted {
            return Ok(());
        }
        return Err(format!("{} exists and does not point to {}", link.display(), target.display()));
    }
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&target, &link).map_err(|e| e.to_string());
    #[cfg(windows)]
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&target)
        .output()
        .map_err(|e| e.to_string())
        .and_then(|o| if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).into_owned()) });
    made.map_err(|e| format!("linking {} to {}: {e}", link.display(), target.display()))
}

// ---------------------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------------------

#[async_trait]
impl ExecutionProfile for JsVitest {
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
        &[NODE]
    }
    fn language(&self) -> &'static str {
        "JavaScript/TypeScript"
    }
    fn prompt_macros(&self) -> &'static str {
        include_str!("../prompts/profiles/js-vitest.md")
    }
    fn is_source_file(&self, path: &Path) -> bool {
        let text = path.to_string_lossy();
        is_source_code(&text)
            || path.file_name().and_then(|n| n.to_str()).is_some_and(|n| self.manifest_names().contains(&n))
    }
    fn source_extensions(&self) -> &'static [&'static str] {
        &SOURCE_EXTENSIONS
    }
    fn manifest_names(&self) -> &'static [&'static str] {
        &[
            "package.json",
            "package-lock.json",
            "pnpm-lock.yaml",
            "yarn.lock",
            "tsconfig.json",
            "vitest.config.ts",
            "vitest.config.js",
            "vitest.config.mjs",
            "vite.config.ts",
            "vite.config.js",
            "vite.config.mjs",
        ]
    }
    fn generated_dirs(&self) -> &'static [&'static str] {
        &GENERATED_DIRS
    }
    fn is_test_path(&self, path: &str) -> bool {
        is_test_path(path)
    }
    async fn describe_repository(&self, worktree: &Path, timeout: Duration, cancel: &CancellationToken) -> Value {
        let manifest = read_json(&worktree.join("package.json")).unwrap_or(Value::Null);
        let lockfile = ["pnpm-lock.yaml", "yarn.lock", "package-lock.json", "bun.lockb", "bun.lock"]
            .into_iter()
            .find(|name| worktree.join(name).is_file());
        let config = has_config(worktree, &["vitest.config", "vitest.workspace", "vite.config"]);
        let mut description = json!({
            "package": manifest.get("name"),
            "type": manifest.get("type"),
            "test_script": manifest.get("scripts").and_then(|s| s.get("test")),
            "lockfile": lockfile,
            "config_file": config,
            "typescript": worktree.join("tsconfig.json").is_file(),
            "test_files_named": "*.test.<ext> / *.spec.<ext>",
        });
        let spec = CommandSpec::new(NODE, std::env::temp_dir()).arg("--version").scrubbed();
        description["node"] = match proc::run(&spec, timeout, cancel).await {
            Ok(out) if out.success() => json!(out.combined().trim()),
            Ok(out) => json!({ "error": crate::stages::context::cap(out.combined().trim(), 2000) }),
            Err(e) => json!({ "error": format!("{e:#}") }),
        };
        description
    }

    fn validate_record(&self, rec: &ReproRecord) -> Result<(), String> {
        if rec.registration_file.as_deref().is_some_and(|r| !r.trim().is_empty()) {
            return Err("Vitest discovers test modules by name; registration_file must be null".into());
        }
        validate_test_file(&rec.test_file)?;
        name_segments(&rec.test_name).map(|_| ())
    }
    fn regression_command_line(&self, rec: &ReproRecord) -> String {
        match (validate_test_file(&rec.test_file), name_segments(&rec.test_name)) {
            (Ok(file), Ok(segments)) => regression_line(&file, &segments),
            _ => String::new(),
        }
    }
    fn regression_command(&self, rec: &ReproRecord, cwd: &Path) -> Result<CommandSpec, String> {
        let file = validate_test_file(&rec.test_file)?;
        let segments = name_segments(&rec.test_name)?;
        Ok(recorder_spec(cwd, &file, &name_pattern(&segments)).shown(regression_line(&file, &segments)))
    }
    fn discovery(&self, spec: &CommandSpec) -> Discovery {
        let file = spec.args.get(3).cloned().unwrap_or_default();
        let mut discovery = recorder_spec(&spec.cwd, &file, "");
        discovery.env = spec.env.clone();
        discovery.clear_env = spec.clear_env;
        discovery.env.push(("LYRA_VITEST_MODE".into(), "discover".into()));
        Discovery {
            spec: discovery.shown(format!("npx vitest run {file}")),
            note: "Discovery runs the whole recorded test file through the recorder with no name filter; once the report is complete it exits 0, so an empty file is evidence rather than a failure.",
        }
    }
    fn format_test(&self, _worktree: &Path, _test_file: &Path) -> Option<CommandSpec> {
        None
    }
    fn classify(&self, output: &str, rec: &ReproRecord) -> RegressionOutcome {
        classify_report(output, rec)
    }
    fn describe_outcome(&self, outcome: &RegressionOutcome, rec: &ReproRecord) -> String {
        describe_run(outcome, rec)
    }
    fn confirm_failure(&self, run: &CommandOutput, rec: &ReproRecord) -> Result<(), String> {
        confirm_failure_in(run, rec)
    }
    fn assert_passed(&self, output: &str, rec: &ReproRecord) -> Result<(), String> {
        assert_passed_in(output, rec)
    }
    fn loaded_test_inputs(&self, output: &str, rec: &ReproRecord, worktree: &Path) -> Result<Vec<String>, String> {
        let read = worktree_reader(worktree);
        loaded_inputs(output, &guards::norm_path(&rec.test_file), &ProductionCode::declared(&read))
    }
    fn assert_absent_from_discovery(&self, output: &str, rec: &ReproRecord) -> Result<(), String> {
        assert_absent(output, rec)
    }
    fn check_repro_changes(
        &self,
        changes: &[(String, String)],
        rec: &ReproRecord,
        _read_old: FileReader<'_>,
        _read_new: FileReader<'_>,
    ) -> Result<(), String> {
        check_repro_changes_with(changes, rec)
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
    fn link_dependencies(&self, source_repo: &Path, checkout_parent: &Path) -> Result<(), String> {
        link_node_modules(source_repo, checkout_parent)
    }
    fn reproduction_environment(&self, spec: CommandSpec, run_root: &Path) -> CommandSpec {
        controlled_environment(spec, &run_root.join("caches"))
    }
    fn check_environment_root(&self, _checkout: &Path) -> Result<(), String> {
        Ok(())
    }
    fn create_environment(&self, env_root: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(env_root.join("caches"))
    }
    fn check_environment(&self, env_root: &Path) -> Result<(), String> {
        if env_root.join("node_modules").join("vitest").is_dir() {
            Ok(())
        } else {
            Err(format!(
                "{} does not link the repository's installed dependencies",
                env_root.join("node_modules").display()
            ))
        }
    }
    fn command_environment(&self, spec: CommandSpec, env_root: &Path) -> CommandSpec {
        controlled_environment(spec, &env_root.join("caches"))
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
        // The suite is mandatory: an explicitly empty command is an error, not a skip. By
        // default the recorder runs every test file, so Vite's cache stays in the run.
        let tests = match &verify.test {
            Some(command) => line("tests", command)?,
            None => recorder_spec(worktree, "", "").shown("npx vitest run".to_string()),
        };
        steps.push(VerifyStep { name: "tests", spec: tests, log: "50-verify/tests.txt", regression: false });
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
        if let Some(line) = output.lines().find_map(|l| l.trim().strip_prefix("LYRA-STARTUP-ERROR ")) {
            return format!("Vitest did not start: {}", line.lines().next().unwrap_or(line));
        }
        let first = output.lines().map(str::trim).find(|l| {
            l.starts_with("FAIL ") || l.starts_with("Error:") || l.contains("AssertionError") || l.starts_with("×")
        });
        match (first, exit_code) {
            (Some(line), _) => line.to_string(),
            (None, Some(code)) => format!("exited with status {code}"),
            (None, None) => "terminated without an exit status".into(),
        }
    }
}

#[cfg(test)]
mod tests;
