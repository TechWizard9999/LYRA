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
use std::path::{Path, PathBuf};
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

/// The recorder: runs Vitest 3 in-process through its Node API and, after the run, prints its
/// evidence in one write: one `LYRA-RESULT` JSON line, then every module the run loaded from
/// inside the checkout (`LYRA-LOADED`) and every one loaded from anywhere other than the
/// checkout and the run's own `node_modules` (`LYRA-FOREIGN`), then `LYRA-END`. The parser
/// accepts exactly one result line and one end marker, so a test printing a result-shaped line
/// makes the run inconclusive rather than forging it.
///
/// Arguments after `--`: test file, name pattern, config file (each may be empty). The test
/// file is selected exactly (Vitest's own file filter matches substrings). With a test file,
/// the result lists every executed test and every test the pattern matches (discovery: every
/// test in the file); without one (the suite) it holds only counts, so a large suite stays a
/// short line. Errors are capped. `LYRA_VITEST_CACHE` moves Vite's cache into the run;
/// `LYRA_VITEST_MODE=discover` exits 0 once the report is complete. Otherwise the exit status
/// is 1 when any test failed, any file, suite hook or unhandled error was reported or nothing
/// ran, 2 when Vitest did not start, else 0.
const RECORDER: &str = r#"import fs from 'node:fs'
import path from 'node:path'
import { createRequire } from 'node:module'
const [file, pattern, config] = process.argv.slice(1)
const mode = process.env.LYRA_VITEST_MODE
const root = fs.realpathSync(process.cwd())
const win = process.platform === 'win32'
const key = p => win ? p.toLowerCase() : p
const inside = (p, dir) => key(p) === key(dir) || key(p).startsWith(key(dir) + path.sep)
const rel = p => path.relative(root, p).split(path.sep).join('/')
const real = p => { try { return fs.realpathSync(p) } catch { return null } }
const deps = real(path.join(path.dirname(root), 'node_modules'))
const text = e => String(e && (e.message || e.stack) || e).slice(0, 2000)
const errs = r => (r && r.errors || []).slice(0, 10).map(text)
let vitest = null
let startup = null
let version = null
try {
  version = JSON.parse(fs.readFileSync(createRequire(path.join(root, 'package.json')).resolve('vitest/package.json'), 'utf8')).version
  if (!/^3\./.test(version)) startup = 'Vitest ' + version + ' is installed; the js-vitest profile supports Vitest 3.x'
} catch (error) { startup = 'cannot resolve vitest: ' + text(error) }
if (!startup) {
  try {
    const { createVitest } = await import('vitest/node')
    process.env.TEST = 'true'
    process.env.VITEST = 'true'
    process.env.NODE_ENV ??= 'test'
    vitest = await createVitest('test', {
      run: true, watch: false, color: false, passWithNoTests: true,
      testNamePattern: pattern || undefined, config: config || undefined, reporters: ['default'],
    }, { cacheDir: process.env.LYRA_VITEST_CACHE || undefined, logLevel: 'error' })
    await vitest.init()
    let specs = await vitest.globTestSpecifications(file ? [file] : [])
    if (file) {
      const wanted = real(path.join(root, file))
      specs = specs.filter(s => wanted && real(s.moduleId) && key(real(s.moduleId)) === key(wanted))
    }
    if (specs.length) await vitest.runTestSpecifications(specs, !file)
  } catch (error) {
    startup = text(error && error.stack || error)
  }
}
const started = !!vitest && !startup
const files = vitest ? vitest.state.getFiles() : []
let matcher = null
try { matcher = pattern ? new RegExp(pattern) : null } catch {}
const tests = []
const suiteErrors = []
const counts = { passed: 0, failed: 0, other: 0 }
const walk = (task, names) => {
  if (task.type === 'test') {
    const state = task.result ? task.result.state : null
    if (state === 'pass') counts.passed++
    else if (state === 'fail') counts.failed++
    else counts.other++
    const full = [...names, task.name]
    if (file && (mode === 'discover' || state === 'pass' || state === 'fail' || (matcher && matcher.test(full.join(' ')))))
      tests.push({ file: rel(task.file ? task.file.filepath : ''), name: full.join(' > '), mode: task.mode, state, errors: errs(task.result) })
    return
  }
  const suite = task.type === 'suite' && task.file !== task
  if (suite && errs(task.result).length)
    suiteErrors.push({ file: rel(task.file ? task.file.filepath : ''), name: [...names, task.name].join(' > '), errors: errs(task.result) })
  for (const child of task.tasks || []) walk(child, suite ? [...names, task.name] : names)
}
for (const f of files) walk(f, [])
const fileErrors = files.filter(f => errs(f.result).length).map(f => ({ file: rel(f.filepath), errors: errs(f.result) }))
const unhandled = vitest ? vitest.state.getUnhandledErrors().length : 0
const loaded = new Set()
const setup = new Set()
let graph = started
for (const project of vitest ? vitest.projects : []) {
  for (const p of [].concat(project.config.setupFiles || [], project.config.globalSetup || [])) {
    const r = typeof p === 'string' ? real(path.resolve(project.config.root, p)) : null
    if (r && inside(r, root)) setup.add(rel(r))
  }
  const modules = project.vite && project.vite.moduleGraph && project.vite.moduleGraph.idToModuleMap
  if (!modules) graph = false
  else for (const id of modules.keys()) loaded.add(id)
}
const lines = ['LYRA-RESULT ' + JSON.stringify({
  started, version, graph, suite: !file, collected: file ? files.map(f => rel(f.filepath)) : [], tests, counts,
  fileErrors, suiteErrors, unhandled, setup: [...setup],
})]
if (file) {
  for (const id of loaded) {
    if (typeof id !== 'string' || id.startsWith('\0') || !path.isAbsolute(id.split('?')[0])) continue
    const r = real(id.split('?')[0])
    if (!r) continue
    if (inside(r, root)) lines.push('LYRA-LOADED ' + rel(r))
    else if (!(deps && inside(r, deps))) lines.push('LYRA-FOREIGN ' + r)
  }
}
lines.push('LYRA-END')
if (vitest) await vitest.close().catch(() => {})
if (startup) console.error('LYRA-STARTUP-ERROR ' + startup.split('\n')[0])
const failed = counts.failed || fileErrors.length || suiteErrors.length || unhandled || !(counts.passed + counts.failed)
const code = !started ? 2 : mode === 'discover' ? 0 : failed ? 1 : 0
process.stdout.write(lines.join('\n') + '\n', () => process.exit(code))"#;
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

/// Path components compare case-insensitively: on the default Windows and macOS filesystems
/// `Node_Modules` or `Tests` is the same directory as `node_modules` or `tests`, so every rule
/// that names a file or directory matches any case, on every platform (stricter, never looser).
fn lower(path: &str) -> String {
    path.to_ascii_lowercase()
}

fn has_component(path: &str, names: &[&str]) -> bool {
    lower(path).split('/').any(|c| names.contains(&c))
}

fn extension(path: &str) -> Option<String> {
    let name = guards::base_name(path);
    name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase())
}

fn is_source_code(path: &str) -> bool {
    extension(path).is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext.as_str()))
}

/// A test-named module: `*.test.*` / `*.spec.*` with a source extension.
fn is_test_module(path: &str) -> bool {
    let name = lower(guards::base_name(path));
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
    let path = lower(path);
    is_test_module(&path)
        || is_setup_file(&path)
        || guards::base_name(&path).ends_with(".snap")
        || path.split('/').rev().skip(1).any(|dir| TEST_DIRS.contains(&dir))
}

pub fn is_test_path(path: &str) -> bool {
    is_test_infrastructure(path)
}

/// Configuration and manifests a fix may never change, wherever they are: every
/// `package.json`, lockfiles, package-manager settings, TypeScript/Babel/SWC settings and
/// every Vitest or Vite configuration; at the root also any `*.config.*` file.
fn configuration_violation(path: &str) -> Option<&'static str> {
    let path = lower(path);
    let name = guards::base_name(&path);
    let root = !path.contains('/');
    if has_component(&path, &["node_modules"]) {
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

/// What the repository declares as its production code, each declaration at its own scope
/// (a file, or a directory and everything below it) and only when it exists in the frozen
/// checkout: the `package.json` `files` entries, the `rootDir` and `include` entries of
/// `tsconfig.json`, and `sources = [...]` under `[profiles.js-vitest]` in the repository's
/// `lyra.toml`, each exactly as written (a glob declares its literal directory prefix,
/// `src/app/**/*.ts` declares `src/app`); and the directory holding each target of `main`,
/// `module`, `browser`, `types`, `typings`, `bin` and `exports` (an entry point at the root
/// declares only that file). Build output named by an entry point (`dist/`, `build/`, `out/`)
/// is not code to edit; wildcard-only entries declare nothing; a declaration under a test,
/// fixture or dependency directory declares nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProductionCode {
    roots: BTreeSet<String>,
    missing: BTreeSet<String>,
}

/// The literal path a declaration names: normalized, cut before its first glob segment.
fn declared_path(raw: &str) -> Option<String> {
    let raw = raw.trim().replace('\\', "/");
    let raw = raw.trim_start_matches("./").trim_start_matches('/');
    let mut kept = Vec::new();
    for segment in raw.split('/') {
        if segment.contains(['*', '?', '{', '[', '!']) {
            break;
        }
        match segment {
            "" | "." => continue,
            ".." => return None,
            s => kept.push(s),
        }
    }
    (!kept.is_empty()).then(|| kept.join("/"))
}

/// The scope an entry point declares: its directory, or the file itself at the root.
fn entry_scope(raw: &str, read: FileReader<'_>) -> Option<String> {
    let path = declared_path(raw)?;
    if read(&format!("{path}/")).is_some() {
        return Some(path);
    }
    Some(path.rsplit_once('/').map_or(path.clone(), |(dir, _)| dir.to_string()))
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
    without_trailing_commas(&out)
}

/// Drops a comma followed (after whitespace) by `}` or `]`, outside strings only.
fn without_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(&next) = chars.get(i + 1) {
                    out.push(next);
                    i += 1;
                }
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == ',' && chars[i + 1..].iter().find(|c| !c.is_whitespace()).is_some_and(|c| matches!(c, '}' | ']'))
        {
            // a trailing comma: dropped
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
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
        explicit.extend(repository_profile_list(read, "sources"));
        for raw in entry_points {
            if let Some(scope) = entry_scope(&raw, read) {
                let top = scope.split('/').next().unwrap_or_default().to_ascii_lowercase();
                if !BUILD_OUTPUT.contains(&top.as_str()) {
                    code.add(&scope, read);
                }
            }
        }
        for raw in explicit {
            if let Some(path) = declared_path(&raw) {
                code.add(&path, read);
            }
        }
        code
    }

    fn add(&mut self, path: &str, read: FileReader<'_>) {
        if has_component(path, &TEST_DIRS)
            || has_component(path, &GENERATED_DIRS)
            || configuration_violation(path).is_some()
        {
            return;
        }
        if read(path).is_some() || read(&format!("{path}/")).is_some() {
            self.roots.insert(path.to_string());
        } else {
            self.missing.insert(path.to_string());
        }
    }

    fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    fn declares(&self, path: &str) -> bool {
        self.roots
            .iter()
            .any(|root| path == root || path.strip_prefix(root.as_str()).is_some_and(|r| r.starts_with('/')))
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
    if path.is_empty() || path.split('/').any(|c| c == ".." || c.is_empty()) || has_component(&path, &["node_modules"])
    {
        return Err(format!("test_file `{test_file}` must be a repository-relative path"));
    }
    if !is_test_module(&path) {
        return Err(format!("test_file `{path}` must be a Vitest test module named `*.test.<ext>` or `*.spec.<ext>`"));
    }
    Ok(path)
}

/// Vitest's `-t` pattern for the full name. Vitest matches the suite and test names joined by
/// a space; a title may itself contain ` > `, so each ` > ` in the recorded name matches either
/// a separator or itself. The pattern is anchored and escaped; if it still selects another test
/// as well, more than one test executes and the run is rejected.
fn name_pattern(name: &str) -> String {
    format!("^{}$", regex::escape(name).replace(" > ", "(?: | > )"))
}

/// The full name in its recorded form, segments trimmed and joined by ` > `.
fn full_name(segments: &[String]) -> String {
    segments.join(" > ")
}

/// POSIX shell quoting for the displayed command: plain words stay as they are.
fn sh_quote(word: &str) -> String {
    if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "_./=:@%+,-".contains(c)) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// The one-line, copyable command shown in `repro.json` and the pull request, quoted for a
/// POSIX shell (`sh`, `bash`, `zsh`). LYRA itself runs the recorder, which selects the test file
/// exactly; Vitest's own file filter also matches longer paths that contain it.
fn regression_line(test_file: &str, name: &str, config: Option<&str>) -> String {
    let mut line = format!("npx vitest run {} -t {}", sh_quote(test_file), sh_quote(&name_pattern(name)));
    if let Some(config) = config {
        line.push_str(&format!(" --config {}", sh_quote(config)));
    }
    line
}

fn suite_line(test_file: Option<&str>, config: Option<&str>) -> String {
    let mut line = "npx vitest run".to_string();
    if let Some(file) = test_file {
        line.push_str(&format!(" {}", sh_quote(file)));
    }
    if let Some(config) = config {
        line.push_str(&format!(" --config {}", sh_quote(config)));
    }
    line
}

/// `node --input-type=module -e <recorder> -- <file> <pattern> <config>`: `--` ends Node's
/// options, so a selector such as `--sample.test.js` stays an argument.
fn recorder_spec(cwd: &Path, file: &str, pattern: &str, config: Option<&str>) -> CommandSpec {
    CommandSpec::new(NODE, cwd)
        .args(["--input-type=module", "-e", RECORDER, "--"])
        .arg(file)
        .arg(pattern)
        .arg(config.unwrap_or_default())
}

/// `key` under `[profiles.js-vitest]` in the repository's `lyra.toml`.
fn repository_profile_value(read: FileReader<'_>, key: &str) -> Option<toml::Value> {
    let doc = read("lyra.toml")?.parse::<toml::Value>().ok()?;
    doc.get("profiles")?.get(ID)?.get(key).cloned()
}

fn repository_profile_list(read: FileReader<'_>, key: &str) -> Vec<String> {
    repository_profile_value(read, key)
        .and_then(|v| v.as_array().cloned())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
}

/// The Vitest configuration file both the reproduction and verification use: `config = "..."`
/// under `[profiles.js-vitest]` in the repository's `lyra.toml`, or Vitest's own default lookup.
/// A `test` script that selects another configuration (`--config` / `-c`) without that setting
/// is an error: the recorder would otherwise run with a different configuration than the suite.
fn vitest_config(read: FileReader<'_>) -> Result<Option<String>, String> {
    if let Some(value) = repository_profile_value(read, "config") {
        let raw = value.as_str().unwrap_or_default();
        let path = guards::norm_path(raw);
        if raw.trim().starts_with(['/', '\\'])
            || raw.chars().nth(1) == Some(':')
            || path.is_empty()
            || path.split('/').any(|c| c == "..")
            || read(&path).is_none()
        {
            return Err(format!(
                "`config = {value}` under [profiles.{ID}] in lyra.toml must name an existing configuration file, \
                 relative to the repository root"
            ));
        }
        return Ok(Some(path));
    }
    let script = read("package.json")
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|m| m.get("scripts")?.get("test")?.as_str().map(String::from))
        .unwrap_or_default();
    let words: Vec<&str> = script.split_whitespace().collect();
    if words.iter().any(|w| *w == "--config" || *w == "-c" || w.starts_with("--config=")) {
        return Err(format!(
            "the package's test script (`{script}`) selects a Vitest configuration file; LYRA runs Vitest with the \
             same file only when it is declared: add `config = \"<path>\"` under [profiles.{ID}] in the \
             repository's lyra.toml"
        ));
    }
    Ok(None)
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
    started: bool,
    /// Every project exposed the module graph the run used.
    graph: bool,
    /// The test files Vitest collected (only when a test file was selected).
    collected: Vec<String>,
    tests: Vec<TestResult>,
    /// `(file, errors)` for file-level errors (collection, setup files, an empty file).
    file_errors: Vec<(String, Vec<String>)>,
    /// `(file, errors)` for errors thrown by a suite's own hooks (`beforeAll`, `afterAll`).
    suite_errors: Vec<(String, Vec<String>)>,
    unhandled: u64,
    /// Setup files and global setup modules the resolved configuration names, inside the checkout.
    setup: Vec<String>,
    loaded: Vec<String>,
    foreign: Vec<String>,
}

/// The recorder's evidence. It is complete only with exactly one result line and exactly one
/// end marker after it; module records count only between the two. A second result-shaped
/// line or end marker anywhere in the output (a test printing one, on either stream) makes the
/// evidence inconclusive instead of replacing it.
fn parse_report(output: &str) -> Option<Report> {
    let lines: Vec<&str> = output.lines().map(str::trim).collect();
    let results: Vec<usize> = (0..lines.len()).filter(|&i| lines[i].starts_with(RESULT_PREFIX)).collect();
    let ends: Vec<usize> = (0..lines.len()).filter(|&i| lines[i] == END_MARKER).collect();
    let ([result], [end]) = (results.as_slice(), ends.as_slice()) else {
        return None;
    };
    if result >= end {
        return None;
    }
    let value: Value = serde_json::from_str(lines[*result].strip_prefix(RESULT_PREFIX)?).ok()?;
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
    let errors = |key: &str| -> Vec<(String, Vec<String>)> {
        value
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|f| (text(f, "file"), strings(f.get("errors"))))
            .collect()
    };
    let block = &lines[result + 1..*end];
    let records = |prefix: &str| -> Vec<String> {
        block.iter().filter_map(|l| l.strip_prefix(prefix)).map(|p| p.trim().to_string()).collect()
    };
    Some(Report {
        started: value.get("started").and_then(Value::as_bool).unwrap_or(false),
        graph: value.get("graph").and_then(Value::as_bool).unwrap_or(false),
        collected: strings(value.get("collected")).iter().map(|p| guards::norm_path(p)).collect(),
        tests,
        file_errors: errors("fileErrors"),
        suite_errors: errors("suiteErrors"),
        unhandled: value.get("unhandled").and_then(Value::as_u64).unwrap_or(0),
        setup: strings(value.get("setup")).iter().map(|p| guards::norm_path(p)).collect(),
        loaded: records(LOADED_PREFIX).iter().map(|p| guards::norm_path(p)).collect(),
        foreign: records(FOREIGN_PREFIX),
    })
}

fn executed(t: &TestResult) -> bool {
    matches!(t.state.as_deref(), Some("pass") | Some("fail"))
}

fn identity(rec: &ReproRecord) -> Option<(String, String)> {
    let file = validate_test_file(&rec.test_file).ok()?;
    let name = full_name(&name_segments(&rec.test_name).ok()?);
    Some((file, name))
}

impl Report {
    fn selected(&self, file: &str, name: &str) -> Option<&TestResult> {
        self.tests.iter().find(|t| t.file == file && t.name == name)
    }

    /// The run observed the modules it used and none came from outside the checkout and the
    /// run's own dependencies.
    fn provenance(&self, file: &str) -> Result<(), String> {
        if !self.graph || !self.loaded.iter().any(|p| p == file) {
            return Err(format!(
                "the Vitest recorder could not observe the modules the run loaded (the module graph is missing or \
                 does not contain {file}); LYRA supports Vitest 3.x"
            ));
        }
        if !self.foreign.is_empty() {
            return Err(format!(
                "the run loaded code from outside the checkout under test and its own node_modules ({}); a workspace \
                 or a link in node_modules points into another directory, so the run did not test the candidate's code",
                self.foreign.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ")
            ));
        }
        Ok(())
    }
}

pub fn classify_report(output: &str, rec: &ReproRecord) -> RegressionOutcome {
    let Some(report) = parse_report(output) else {
        return RegressionOutcome::Other("the Vitest recorder did not report exactly one complete result".into());
    };
    let Some((file, name)) = identity(rec) else {
        return RegressionOutcome::Other("the reproduction record does not name a valid test".into());
    };
    if !report.started {
        return RegressionOutcome::SetupFailed;
    }
    if report.unhandled > 0
        || report.file_errors.iter().any(|(f, _)| *f == file)
        || report.suite_errors.iter().any(|(f, _)| *f == file)
    {
        return RegressionOutcome::SetupFailed;
    }
    let Some(test) = report.selected(&file, &name) else {
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

/// Exactly the selected test executed, nothing else went wrong, and the run's provenance holds.
fn exactly_ours(report: &Report, file: &str, name: &str) -> Result<(), String> {
    let ran: Vec<&TestResult> = report.tests.iter().filter(|t| executed(t)).collect();
    if ran.len() != 1 || ran[0].file != file || ran[0].name != name {
        return Err(format!(
            "exactly one test must execute, `{file} > {name}`; {} ran ({})",
            ran.len(),
            ran.iter().map(|t| format!("`{} > {}`", t.file, t.name)).collect::<Vec<_>>().join(", ")
        ));
    }
    if report.unhandled > 0 || !report.file_errors.is_empty() || !report.suite_errors.is_empty() {
        return Err("the run reported file-level, suite hook or unhandled errors".into());
    }
    report.provenance(file)
}

/// S2: exactly the selected test failed, with Vitest's test-failure status, and its own
/// assertion error (not merely the output) carries the expected text.
pub fn confirm_failure_in(run: &CommandOutput, rec: &ReproRecord) -> Result<(), String> {
    let output = run.combined();
    let (file, name) = identity(rec).ok_or("the reproduction record does not name a valid test")?;
    let report = parse_report(&output).ok_or("the Vitest recorder did not report exactly one complete result")?;
    exactly_ours(&report, &file, &name)?;
    let test = report.selected(&file, &name).filter(|t| t.state.as_deref() == Some("fail"));
    let Some(test) = test else {
        return Err("the selected test did not fail".into());
    };
    if run.exit_code != Some(1) {
        return Err("reproduction must exit with Vitest's test-failure status (1)".into());
    }
    if !guards::output_contains_expected(&test.errors.join("\n"), &rec.expected) {
        return Err(format!(
            "the selected test failed, but its own error does not contain the expected text {:?} (text printed by the \
             test is not evidence): {}",
            rec.expected.trim(),
            test.errors.first().map(String::as_str).unwrap_or("no error recorded")
        ));
    }
    Ok(())
}

/// S5: exactly the selected test passed, and the run used the candidate's own code.
pub fn assert_passed_in(output: &str, rec: &ReproRecord) -> Result<(), String> {
    let (file, name) = identity(rec).ok_or("the reproduction record does not name a valid test")?;
    let report = parse_report(output).ok_or("the Vitest recorder did not report exactly one complete result")?;
    exactly_ours(&report, &file, &name)?;
    match report.selected(&file, &name).and_then(|t| t.state.as_deref()) {
        Some("pass") => Ok(()),
        other => Err(format!("the selected test did not pass ({})", other.unwrap_or("not run"))),
    }
}

/// The error Vitest may report for a test module that defines no test (the emptied file).
const NO_SUITE: &str = "No test suite found in file";

/// With the test file emptied, the recorder's report must be complete, show that Vitest
/// collected that file, list no test from it, and report no error other than the file being
/// empty: a collection, setup or hook error is a failed discovery, not evidence of absence.
pub fn assert_absent(output: &str, rec: &ReproRecord) -> Result<(), String> {
    let (file, _) = identity(rec).ok_or("the reproduction record does not name a valid test")?;
    let report = parse_report(output).ok_or("discovery did not report exactly one complete result")?;
    if !report.started {
        return Err("Vitest did not start for discovery".into());
    }
    let found: Vec<&TestResult> = report.tests.iter().filter(|t| t.file == file).collect();
    if !found.is_empty() {
        return Err(format!(
            "with {file} emptied, Vitest still found {} test(s) in it: the selected test is not bound to this source",
            found.len()
        ));
    }
    let empty = |(f, errors): &(String, Vec<String>)| *f == file && errors.iter().all(|e| e.contains(NO_SUITE));
    let other: Vec<String> = report
        .file_errors
        .iter()
        .filter(|e| !empty(e))
        .chain(&report.suite_errors)
        .map(|(f, errors)| format!("{f}: {}", errors.first().map(String::as_str).unwrap_or_default()))
        .collect();
    if !other.is_empty() || report.unhandled > 0 {
        return Err(format!(
            "discovery with {file} emptied failed instead of finding no test ({}{})",
            other.join("; "),
            if report.unhandled > 0 { format!("; {} unhandled error(s)", report.unhandled) } else { String::new() }
        ));
    }
    if !report.collected.contains(&file) {
        return Err(format!("discovery did not collect {file}: Vitest's configuration does not include it"));
    }
    Ok(())
}

/// The inputs to freeze with the test: every non-production module the failing run loaded and
/// every setup file the resolved configuration names (frozen by path, whatever its name, also
/// inside production code). A module from outside the checkout and the run's dependencies is
/// an error (the regression did not run the candidate's code), and so is a module the
/// repository's declarations cannot classify.
pub fn loaded_inputs(output: &str, test_file: &str, production: &ProductionCode) -> Result<Vec<String>, String> {
    let report = parse_report(output).ok_or(
        "the regression run did not report the modules it loaded, so LYRA cannot establish which test inputs to lock",
    )?;
    report.provenance(test_file)?;
    let mut frozen = BTreeSet::new();
    let mut unknown = BTreeSet::new();
    let usable = |path: &String| !path.is_empty() && path != test_file && !path.split('/').any(|c| c == "..");
    for path in report.setup.iter().filter(|p| usable(p)) {
        if !has_component(path, &["node_modules"]) {
            frozen.insert(path.clone());
        }
    }
    for path in report.loaded.iter().filter(|p| usable(p)) {
        if has_component(path, &["node_modules"]) {
            continue;
        }
        match production.classify(path) {
            Classification::Production => {}
            Classification::TestSupport => {
                frozen.insert(path.clone());
            }
            Classification::Unknown if !frozen.contains(path) => {
                unknown.insert(path.clone());
            }
            Classification::Unknown => {}
        }
    }
    if !unknown.is_empty() {
        return Err(production.unknown_message(&unknown));
    }
    Ok(frozen.into_iter().collect())
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
        match vitest_config(read_checkpoint) {
            Ok(config) => {
                let expected = regression_line(&file, &name, config.as_deref());
                if rec.command.trim() != expected {
                    violations.push(format!(
                        "recorded test command changed: {:?} (expected {expected:?})",
                        rec.command.trim()
                    ));
                }
            }
            Err(reason) => violations.push(reason),
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

/// The Vitest major version the recorder supports: it reads Vitest 3's module graph
/// (`project.vite.moduleGraph`); Vitest 2 and 4 keep the runner's modules elsewhere.
const SUPPORTED_VITEST_MAJOR: &str = "3";

/// Files whose content decides what is installed in `node_modules`.
const INSTALL_INPUTS: [&str; 8] = [
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    ".npmrc",
];

/// Gives the checkout its own copy of the operator's installed dependencies, at
/// `checkout/../node_modules` where Node's upward resolution finds it. Nothing inside the
/// checkout changes, and nothing the run writes there reaches the operator's install: tests
/// and verification commands may write to `node_modules`, so a link would let them change the
/// operator's packages behind the guards, which see only the checkout.
///
/// Fails closed, with the step that fixes it, when the checkout is a workspace, Vitest is not
/// installed or is not 3.x, or the install was made for a different `package.json` / lockfile
/// than the checkout's (the operator's clone is on another branch): verification would run the
/// candidate against dependencies its own manifests do not describe.
pub fn provision_node_modules(source_repo: &Path, checkout: &Path) -> Result<(), String> {
    for (dir, which) in [(checkout, "the checkout"), (source_repo, "the repository")] {
        if let Some(kind) = workspaces(dir) {
            return Err(format!(
                "{which} is a JavaScript workspace ({kind}); LYRA supports one package at a time and workspaces are \
                 not supported yet"
            ));
        }
    }
    let source = source_repo.join("node_modules");
    let installed = std::fs::read_to_string(source.join("vitest").join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|m| m.get("version").and_then(Value::as_str).map(String::from));
    let Some(version) = installed else {
        return Err(format!(
            "{} has no installed Vitest; install the repository's dependencies first (`npm ci`, \
             `pnpm install --frozen-lockfile` or `yarn install --frozen-lockfile`) and retry",
            source.display()
        ));
    };
    if version.split('.').next() != Some(SUPPORTED_VITEST_MAJOR) {
        return Err(format!(
            "the repository has Vitest {version} installed; the js-vitest profile supports Vitest \
             {SUPPORTED_VITEST_MAJOR}.x, whose module graph it reads to establish which code the test ran"
        ));
    }
    let differing: Vec<&str> = INSTALL_INPUTS
        .into_iter()
        .filter(|name| std::fs::read(source_repo.join(name)).ok() != std::fs::read(checkout.join(name)).ok())
        .collect();
    if !differing.is_empty() {
        return Err(format!(
            "the installed dependencies in {} were installed for a different {} than the commit under test; check \
             out the default branch in {} (or a branch with the same manifests), reinstall, and retry",
            source.display(),
            differing.join(", "),
            source_repo.display()
        ));
    }
    let parent = checkout.parent().ok_or("the checkout has no parent directory")?;
    let target = parent.join("node_modules");
    if std::fs::symlink_metadata(&target).is_ok() {
        remove_tree(&target).map_err(|e| format!("removing {}: {e}", target.display()))?;
    }
    let source = dunce::canonicalize(&source).map_err(|e| format!("{}: {e}", source.display()))?;
    copy_tree((&source, &target), &source, &target, &mut Vec::new())
        .map_err(|e| format!("copying {} to {}: {e}", source.display(), target.display()))
}

fn remove_tree(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path))
    }
}

/// Copies the tree `from` (inside `roots.0`, the canonical source `node_modules`) to `to`
/// (inside `roots.1`, its copy). `std::fs::copy` clones files where the filesystem supports
/// it (APFS, Btrfs, XFS, ReFS), so the copy is cheap there. A link that resolves inside the
/// source is recreated against the copy on Unix; on Windows, where creating links needs
/// privileges, its target is copied instead (`stack` guards against cycles). A link that
/// resolves elsewhere (a workspace or `npm link` package) keeps pointing there, and the
/// recorder reports any module loaded through it as foreign.
fn copy_tree(roots: (&Path, &Path), from: &Path, to: &Path, stack: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let (source_root, copy_root) = roots;
    std::fs::create_dir_all(to)?;
    stack.push(dunce::canonicalize(from)?);
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let dest = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if !kind.is_symlink() {
            if kind.is_dir() {
                copy_tree(roots, &path, &dest, stack)?;
            } else {
                std::fs::copy(&path, &dest)?;
            }
            continue;
        }
        let Ok(resolved) = dunce::canonicalize(&path) else {
            continue; // a dangling link resolves nothing
        };
        let inside = resolved.strip_prefix(source_root).ok().map(|r| copy_root.join(r));
        match inside {
            None => make_link(&resolved, &dest)?,
            Some(mapped) if cfg!(unix) => make_link(&mapped, &dest)?,
            Some(_) if resolved.is_dir() => {
                if !stack.contains(&resolved) {
                    copy_tree(roots, &resolved, &dest, stack)?;
                }
            }
            Some(_) => {
                std::fs::copy(&resolved, &dest)?;
            }
        }
    }
    stack.pop();
    Ok(())
}

#[cfg(unix)]
fn make_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Windows: links need privileges (symbolic links) or a raw reparse-point API (junctions), and
/// a shell such as `cmd /C mklink` would parse the paths, so a link out of `node_modules` is
/// refused: copying it would hide that the run used code from outside the checkout.
#[cfg(windows)]
fn make_link(target: &Path, link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(format!(
        "{} links to {}, outside node_modules (a workspace or `npm link` package); LYRA cannot isolate it",
        link.display(),
        target.display()
    )))
}

#[cfg(not(any(unix, windows)))]
fn make_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::fs::copy(target, link).map(|_| ())
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
    fn regression_command_line(&self, rec: &ReproRecord, worktree: &Path) -> String {
        match (identity(rec), vitest_config(&worktree_reader(worktree))) {
            (Some((file, name)), Ok(config)) => regression_line(&file, &name, config.as_deref()),
            _ => String::new(),
        }
    }
    fn regression_command(&self, rec: &ReproRecord, cwd: &Path) -> Result<CommandSpec, String> {
        let file = validate_test_file(&rec.test_file)?;
        let name = full_name(&name_segments(&rec.test_name)?);
        let config = vitest_config(&worktree_reader(cwd))?;
        Ok(recorder_spec(cwd, &file, &name_pattern(&name), config.as_deref()).shown(regression_line(
            &file,
            &name,
            config.as_deref(),
        )))
    }
    fn discovery(&self, spec: &CommandSpec) -> Discovery {
        let selectors = spec.args.iter().position(|a| a == "--").map_or(&[][..], |i| &spec.args[i + 1..]);
        let file = selectors.first().cloned().unwrap_or_default();
        let config = selectors.get(2).filter(|c| !c.is_empty()).cloned();
        let mut discovery = recorder_spec(&spec.cwd, &file, "", config.as_deref());
        discovery.env = spec.env.clone();
        discovery.clear_env = spec.clear_env;
        discovery.env.push(("LYRA_VITEST_MODE".into(), "discover".into()));
        Discovery {
            spec: discovery.shown(suite_line(Some(&file), config.as_deref())),
            note: "Discovery runs exactly the recorded test file through the recorder with no name filter; once the report is complete it exits 0. It must report the emptied file as having no test suite, and no other error.",
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
    fn provision_dependencies(&self, source_repo: &Path, checkout: &Path) -> Result<(), String> {
        provision_node_modules(source_repo, checkout)
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
                "{} does not hold a copy of the repository's installed dependencies",
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
            None => {
                let config = vitest_config(&worktree_reader(worktree))?;
                recorder_spec(worktree, "", "", config.as_deref()).shown(suite_line(None, config.as_deref()))
            }
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
