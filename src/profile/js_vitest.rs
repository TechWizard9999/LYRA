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
use serde::Deserialize;
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

const RECORDER: &str = include_str!("js_vitest/recorder.mjs");

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

fn workspace_config(dir: &Path) -> Option<String> {
    has_config(dir, &["vitest.workspace", "vitest.projects"]).or_else(|| {
        ["vitest.workspace.json", "vitest.projects.json"]
            .into_iter()
            .find(|name| dir.join(name).is_file())
            .map(str::to_owned)
    })
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
    if let Some(config) = has_config(worktree, &["vitest.config"]).or_else(|| workspace_config(worktree)) {
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

/// Workspace declarations in the frozen checkout and operator's clone.
fn workspaces(worktree: &Path, manifest: Option<&[u8]>) -> Option<&'static str> {
    if worktree.join("pnpm-workspace.yaml").is_file() {
        return Some("pnpm-workspace.yaml");
    }
    if workspace_config(worktree).is_some() {
        return Some("Vitest workspace configuration");
    }
    manifest
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
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

/// What the repository declares as its production code, bounded by the files, directories,
/// and glob patterns that match the frozen checkout: the targets of `package.json` `main`,
/// `module`, `browser`, `types`, `typings`, `bin` and `exports` and its `files` entries, the
/// `rootDir` and `include` entries of `tsconfig.json`, and `sources = [...]` under
/// `[profiles.js-vitest]` in the repository's `lyra.toml`. Build output named by the package
/// entry points (`dist/`, `build/`, `out/`) is not code to edit; wildcard-only entries declare
/// nothing.
#[derive(Debug, Default)]
pub struct ProductionCode {
    included: Vec<ProductionPath>,
    excluded: Vec<ProductionPath>,
    missing: BTreeSet<String>,
    unsupported_exclusions: BTreeSet<String>,
}

#[derive(Debug)]
enum ProductionPath {
    File(String),
    Directory(String),
    Glob(String, regex::Regex),
}

impl ProductionPath {
    fn matches(&self, path: &str) -> bool {
        match self {
            Self::File(file) => path == file,
            Self::Directory(dir) => path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/')),
            Self::Glob(_, pattern) => pattern.is_match(path),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::File(path) | Self::Directory(path) | Self::Glob(path, _) => path,
        }
    }
}

fn declared_path(raw: &str) -> Option<String> {
    let raw = raw.replace('\\', "/");
    let path = raw.trim_start_matches("./").trim_end_matches('/');
    (!path.contains(':')
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| !matches!(part, "" | "." | "..") && !part.eq_ignore_ascii_case(".git")))
    .then(|| path.to_string())
}

fn is_glob(path: &str) -> bool {
    path.contains(['*', '?', '[', '{']) || ["@(", "+(", "!("].iter().any(|operator| path.contains(operator))
}

fn glob_segment(segment: &str, pattern: &mut String) -> Option<()> {
    if ["@(", "+(", "?(", "*(", "!("].iter().any(|operator| segment.contains(operator)) {
        return None;
    }
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' => pattern.push_str("[^/]*"),
            '?' => pattern.push_str("[^/]"),
            '[' => {
                let (class, rest) = chars.as_str().split_once(']')?;
                chars = rest.chars();
                let (class, negated) = class.strip_prefix('!').map_or((class, false), |s| (s, true));
                if class.is_empty()
                    || !class.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
                {
                    return None;
                }
                pattern.push_str(if negated { "[^/" } else { "[" });
                pattern.push_str(class);
                pattern.push(']');
            }
            '{' => {
                let (alternatives, rest) = chars.as_str().split_once('}')?;
                chars = rest.chars();
                if !alternatives.contains(',') || alternatives.contains('{') {
                    return None;
                }
                pattern.push_str("(?:");
                for (index, alternative) in alternatives.split(',').enumerate() {
                    if index > 0 {
                        pattern.push('|');
                    }
                    glob_segment(alternative, pattern)?;
                }
                pattern.push(')');
            }
            _ => pattern.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
        }
    }
    Some(())
}

fn glob_pattern(path: &str) -> Option<regex::Regex> {
    let mut pattern = "^".to_string();
    let mut segments = path.split('/').peekable();
    while let Some(segment) = segments.next() {
        match (segment, segments.peek().is_some()) {
            ("**", true) => pattern.push_str("(?:[^/]+/)*"),
            ("**", false) => pattern.push_str("(?:[^/]+(?:/[^/]+)*)?"),
            (_, more) => {
                glob_segment(segment, &mut pattern)?;
                if more {
                    pattern.push('/');
                }
            }
        }
    }
    pattern.push('$');
    regex::Regex::new(&pattern).ok()
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
    pub fn declared(read: FileReader<'_>, paths: Option<&BTreeSet<String>>) -> Self {
        let mut code = Self::default();
        let mut entry_points = Vec::new();
        let mut explicit = Vec::new();
        let mut excluded = Vec::new();
        if let Some(manifest) = read("package.json").and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            for key in ["main", "module", "browser", "types", "typings", "bin", "exports"] {
                if let Some(value) = manifest.get(key) {
                    strings_in(value, &mut entry_points);
                }
            }
            if let Some(files) = manifest.get("files").and_then(Value::as_array) {
                for file in files.iter().filter_map(Value::as_str) {
                    if file.starts_with("!(") {
                        code.unsupported_exclusions.insert(file.to_string());
                    } else if let Some(exclusion) = file.strip_prefix('!') {
                        excluded.push(exclusion.to_string());
                    } else {
                        explicit.push(file.to_string());
                    }
                }
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
            if let Some(exclude) = tsconfig.get("exclude") {
                strings_in(exclude, &mut excluded);
            }
        }
        explicit.extend(repository_profile_list(read, "sources"));
        for raw in entry_points {
            if let Some(path) = declared_path(&raw).filter(|p| !BUILD_OUTPUT.contains(&p.split('/').next().unwrap())) {
                code.add(path, true, read, paths);
            }
        }
        for raw in explicit {
            if let Some(path) = declared_path(&raw) {
                code.add(path, false, read, paths);
            }
        }
        for raw in excluded {
            let Some(path) = declared_path(&raw) else {
                code.unsupported_exclusions.insert(raw);
                continue;
            };
            if is_glob(&path) {
                if let Some(pattern) = glob_pattern(&path) {
                    code.excluded.push(ProductionPath::Glob(path, pattern));
                } else {
                    code.unsupported_exclusions.insert(path);
                }
            } else {
                code.excluded.push(ProductionPath::File(path.clone()));
                code.excluded.push(ProductionPath::Directory(path));
            }
        }
        code
    }

    fn add(&mut self, path: String, entry_point: bool, read: FileReader<'_>, paths: Option<&BTreeSet<String>>) {
        if is_test_infrastructure(&path) || configuration_violation(&path).is_some() {
            return;
        }
        let prefix = format!("{path}/");
        let exists =
            paths.map_or_else(|| read(&path).is_some() && read(&prefix).is_none(), |paths| paths.contains(&path));
        if exists {
            if is_source_code(&path) {
                self.included.push(ProductionPath::File(path));
            }
        } else if !entry_point
            && paths.map_or_else(
                || read(&prefix).is_some(),
                |paths| {
                    paths
                        .range(prefix.clone()..)
                        .take_while(|p| p.starts_with(&prefix))
                        .any(|p| can_be_declared_source(p))
                },
            )
        {
            self.included.push(ProductionPath::Directory(path));
        } else if let Some(pattern) = path
            .split_once('/')
            .filter(|(base, rest)| !is_glob(base) && is_glob(rest))
            .and_then(|_| glob_pattern(&path))
        {
            if paths
                .is_some_and(|files| files.iter().any(|file| pattern.is_match(file) && can_be_declared_source(file)))
            {
                self.included.push(ProductionPath::Glob(path, pattern));
            } else {
                self.missing.insert(path);
            }
        } else {
            self.missing.insert(path);
        }
    }

    fn is_empty(&self) -> bool {
        self.included.is_empty()
    }

    fn declares(&self, path: &str) -> bool {
        self.included.iter().any(|entry| entry.matches(path)) && !self.excluded.iter().any(|entry| entry.matches(path))
    }

    fn classify(&self, path: &str) -> Classification {
        if is_test_infrastructure(path) || configuration_violation(path).is_some() {
            return Classification::TestSupport;
        }
        if self.is_empty() || !self.unsupported_exclusions.is_empty() {
            return if is_source_code(path) { Classification::Unknown } else { Classification::TestSupport };
        }
        if self.declares(path) {
            Classification::Production
        } else {
            Classification::TestSupport
        }
    }

    fn describe(&self) -> String {
        self.included
            .iter()
            .map(|entry| format!("`{}`", entry.name()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn unknown_message(&self, paths: &BTreeSet<String>) -> String {
        if !self.unsupported_exclusions.is_empty() {
            return format!(
                "cannot determine the editable source paths because the repository uses unsupported exclusion patterns: {}",
                self.unsupported_exclusions.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ")
            );
        }
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

fn can_be_declared_source(path: &str) -> bool {
    is_source_code(path) && !is_test_infrastructure(path) && configuration_violation(path).is_none()
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

/// A ' > '-separated name can escape '>' and '\' to keep literal name characters.
fn name_segments(test_name: &str) -> Result<Vec<String>, String> {
    let name = test_name.trim();
    if name.is_empty() || name.chars().count() > 500 {
        return Err("test_name must be the test's full name, such as describe > test, at most 500 characters".into());
    }
    if name.chars().any(char::is_control) {
        return Err("test_name must not contain control characters".into());
    }
    let segments: Vec<String> = name
        .split(" > ")
        .map(|segment| {
            let mut chars = segment.chars().peekable();
            let mut decoded = String::new();
            while let Some(c) = chars.next() {
                if c == '\\' && chars.peek().is_some_and(|next| matches!(next, '\\' | '>')) {
                    decoded.push(chars.next().unwrap());
                } else {
                    decoded.push(c);
                }
            }
            decoded
        })
        .collect();
    if segments.iter().any(|segment| segment.trim().is_empty()) {
        return Err(format!("test_name {name:?} has an empty segment; use describe > test"));
    }
    Ok(segments)
}

fn validate_test_file(test_file: &str) -> Result<String, String> {
    let raw = test_file.trim();
    if raw.starts_with(['/', '\\']) || raw.chars().nth(1) == Some(':') || raw.chars().any(char::is_control) {
        return Err(format!("test_file {test_file:?} must be a repository-relative path without control characters"));
    }
    let path = guards::norm_path(raw);
    if !valid_relative_path(&path) {
        return Err(format!("test_file {test_file:?} must be a repository-relative path"));
    }
    if !is_test_module(&path) {
        return Err(format!("test_file {path:?} must be a Vitest test module named *.test.<ext> or *.spec.<ext>"));
    }
    Ok(path)
}

fn valid_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !path.split('/').any(|component| matches!(component, "" | "." | ".."))
        && !has_component(path, &["node_modules"])
        && path.chars().nth(1) != Some(':')
}

/// Vitest 3 and 4 use space-separated suite and test names in testNamePattern.
fn name_pattern(segments: &[String]) -> String {
    format!("^{}$", segments.iter().map(|s| regex::escape(s)).collect::<Vec<_>>().join(" "))
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
fn regression_line(test_file: &str, name: &[String], config: Option<&str>) -> String {
    let mut line =
        format!("npx vitest run {} -t {}", sh_quote(&format!("./{test_file}")), sh_quote(&name_pattern(name)));
    if let Some(config) = config {
        line.push_str(&format!(" --config {}", sh_quote(config)));
    }
    line
}

fn suite_line(test_file: Option<&str>, config: Option<&str>) -> String {
    let mut line = "npx vitest run".to_string();
    if let Some(file) = test_file {
        line.push_str(&format!(" {}", sh_quote(&format!("./{file}"))));
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
        .with_separate_report()
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

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ReportMode {
    Regression,
    Discover,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestResult {
    file: String,
    name: Vec<String>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    state: Option<String>,
    errors: Vec<TestError>,
    inverted: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestError {
    name: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteReport {
    version: u32,
    mode: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Report {
    version: u32,
    mode: ReportMode,
    collected: Vec<String>,
    tests: Vec<TestResult>,
    suite_errors: Vec<SuiteError>,
    unhandled: u64,
    loaded: Vec<String>,
    setup: Vec<String>,
    foreign: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteError {
    file: String,
    errors: Vec<TestError>,
}

fn parse_report(evidence: &str, mode: ReportMode) -> Result<Report, String> {
    let report: Report = serde_json::from_str(evidence)
        .map_err(|_| "the Vitest recorder did not produce a complete result; see the command log".to_string())?;
    if report.version != 1 || report.mode != mode {
        return Err("the Vitest recorder produced an incompatible result".into());
    }
    Ok(report)
}

fn executed(test: &TestResult) -> bool {
    matches!(test.state.as_deref(), Some("pass" | "fail"))
}

fn identity(rec: &ReproRecord) -> Option<(String, Vec<String>)> {
    Some((validate_test_file(&rec.test_file).ok()?, name_segments(&rec.test_name).ok()?))
}

fn foreign_check(report: &Report) -> Result<(), String> {
    if report.foreign.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Vitest loaded modules from outside the checkout under test and its isolated node_modules ({})",
        report.foreign.join(", ")
    ))
}

fn verify_provenance(report: &Report, file: &str) -> Result<(), String> {
    if !report.loaded.iter().any(|path| path == file) {
        return Err(format!("the Vitest recorder did not observe {file} in the module graph"));
    }
    foreign_check(report)
}

fn check_suite_errors(report: &Report) -> Result<(), String> {
    if report.unhandled != 0 {
        return Err(format!("Vitest reported {} unhandled error(s)", report.unhandled));
    }
    if let Some(error) = report.suite_errors.first() {
        let messages = error.errors.iter().map(|error| error.message.as_str()).collect::<Vec<_>>();
        return Err(format!("Vitest reported a suite-level error in {}: {}", error.file, messages.join("; ")));
    }
    Ok(())
}

pub fn classify_report(evidence: &str, rec: &ReproRecord) -> RegressionOutcome {
    let report = match parse_report(evidence, ReportMode::Regression) {
        Ok(report) => report,
        Err(error) => return RegressionOutcome::Other(error),
    };
    let Some((file, name)) = identity(rec) else {
        return RegressionOutcome::Other("the reproduction record does not name a valid test".into());
    };
    if check_suite_errors(&report).is_err() {
        return RegressionOutcome::SetupFailed;
    }
    if let Err(error) = verify_provenance(&report, &file) {
        return RegressionOutcome::Other(error);
    }
    let Some(test) = report.tests.iter().find(|test| test.file == file && test.name == name) else {
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

fn selected_test<'a>(report: &'a Report, rec: &ReproRecord) -> Result<&'a TestResult, String> {
    let (file, name) = identity(rec).ok_or("the reproduction record does not name a valid test")?;
    verify_provenance(report, &file)?;
    check_suite_errors(report)?;
    let mut ran = report.tests.iter().filter(|test| executed(test));
    let test = ran.next().filter(|test| test.file == file && test.name == name);
    let test = test
        .filter(|_| ran.next().is_none())
        .ok_or_else(|| format!("exactly one test must execute, {file} > {}", rec.test_name.trim()))?;
    if test.inverted {
        return Err("the selected test uses Vitest's fails modifier".into());
    }
    Ok(test)
}

pub fn confirm_failure_in(run: &CommandOutput, rec: &ReproRecord) -> Result<(), String> {
    let report = parse_report(run.report.as_deref().unwrap_or_default(), ReportMode::Regression)?;
    let test = selected_test(&report, rec)?;
    if test.state.as_deref() != Some("fail") {
        return Err("the selected test did not fail".into());
    }
    if !test
        .errors
        .iter()
        .any(|error| error.name == "AssertionError" && guards::output_contains_expected(&error.message, &rec.expected))
    {
        return Err(format!("the selected test failed without the expected assertion text {:?}", rec.expected.trim()));
    }
    if run.exit_code != Some(1) {
        return Err("reproduction must exit with Vitest's test-failure status (1)".into());
    }
    Ok(())
}

pub fn assert_passed_in(evidence: &str, rec: &ReproRecord) -> Result<(), String> {
    let report = parse_report(evidence, ReportMode::Regression)?;
    let test = selected_test(&report, rec)?;
    if test.state.as_deref() != Some("pass") || !test.errors.is_empty() {
        return Err("the selected test did not pass cleanly".into());
    }
    Ok(())
}

pub fn assert_absent(evidence: &str, rec: &ReproRecord) -> Result<(), String> {
    let (file, _) = identity(rec).ok_or("the reproduction record does not name a valid test")?;
    let report = parse_report(evidence, ReportMode::Discover)?;
    check_suite_errors(&report)?;
    verify_provenance(&report, &file)?;
    if report.collected != [file] {
        return Err("Vitest did not collect the exact recorded test file".into());
    }
    if !report.tests.is_empty() {
        return Err("Vitest still found tests with the recorded source emptied".into());
    }
    Ok(())
}

/// The modules loaded outside declared production code, plus setup and configuration inputs.
pub fn loaded_inputs(evidence: &str, test_file: &str, production: &ProductionCode) -> Result<Vec<String>, String> {
    let report = parse_report(evidence, ReportMode::Regression)?;
    verify_provenance(&report, test_file)?;
    let mut loaded = BTreeSet::new();
    let mut unknown = BTreeSet::new();
    for path in report.setup {
        if !valid_relative_path(&path) {
            return Err(format!("Vitest reported an invalid setup path {path:?}"));
        }
        if path != test_file {
            loaded.insert(path);
        }
    }
    for path in report.loaded {
        if !valid_relative_path(&path) {
            return Err(format!("Vitest reported an invalid module path {path:?}"));
        }
        if path == test_file || loaded.contains(&path) {
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
    checkpoint_paths: Option<&BTreeSet<String>>,
) -> Result<(), String> {
    if changes.is_empty() {
        return Err("fix produced no changes".to_string());
    }
    let config = vitest_config(read_checkpoint)?;
    let mut violations = guards::frozen_test_violations(changes, rec, read_checkpoint, read_now);
    let frozen: BTreeSet<String> =
        std::iter::once(&rec.test_file).chain(&rec.frozen_inputs).map(|p| guards::norm_path(p)).collect();
    let production = ProductionCode::declared(read_checkpoint, checkpoint_paths);
    for (status, raw) in changes {
        let path = guards::norm_path(raw);
        if config.as_deref().is_some_and(|config| config.eq_ignore_ascii_case(&path)) {
            violations.push(format!("{status} {path}: the configured Vitest configuration is locked"));
            continue;
        }
        if frozen.contains(&path) {
            continue;
        }
        if let Some(reason) = fix_may_not_change(&path, &production) {
            violations.push(format!("{status} {path}: {reason}"));
        }
    }
    if let Some((file, name)) = identity(rec) {
        let expected = regression_line(&file, &name, config.as_deref());
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

mod dependencies;

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
        let name = name_segments(&rec.test_name)?;
        let config = vitest_config(&worktree_reader(cwd))?;
        Ok(recorder_spec(cwd, &file, &name_pattern(&name), config.as_deref()).shown(regression_line(
            &file,
            &name,
            config.as_deref(),
        )))
    }
    fn discovery(&self, spec: &CommandSpec) -> Discovery {
        Discovery {
            spec: spec.clone().env("LYRA_VITEST_MODE", "discover").shown("collect the recorded Vitest file"),
            note: "Vitest collects tests from the exact file without running test callbacks.",
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
    fn assert_verification_report(&self, report: &str) -> Result<(), String> {
        let complete = serde_json::from_str::<SuiteReport>(report)
            .is_ok_and(|report| report.version == 1 && report.mode == "suite");
        if complete {
            Ok(())
        } else {
            Err("the Vitest recorder did not complete the suite; see the command log".into())
        }
    }
    fn loaded_test_inputs(&self, output: &str, rec: &ReproRecord, worktree: &Path) -> Result<Vec<String>, String> {
        let tracked = guards::checkpoint_paths(worktree, "HEAD")
            .map_err(|error| format!("cannot list tracked source paths for the Vitest input lock: {error:#}"))?;
        let read = worktree_reader(worktree);
        loaded_inputs(output, &guards::norm_path(&rec.test_file), &ProductionCode::declared(&read, Some(&tracked)))
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
        checkpoint_paths: Option<&BTreeSet<String>>,
    ) -> Result<(), String> {
        check_fix_changes_with(changes, rec, read_checkpoint, read_now, checkpoint_paths)
    }
    async fn provision_dependencies(
        &self,
        source_repo: &Path,
        checkout: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        // Intake reports an undeclared or invalid Vitest config here, before any agent runs.
        vitest_config(&worktree_reader(checkout))?;
        let (source_repo, checkout, cancel) = (source_repo.to_owned(), checkout.to_owned(), cancel.clone());
        tokio::task::spawn_blocking(move || {
            let parent = checkout.parent().ok_or("the checkout has no parent directory")?;
            dependencies::prepare(&source_repo, &checkout, parent, &cancel)
        })
        .await
        .map_err(|error| format!("preparing isolated Vitest dependencies: {error}"))?
    }
    fn reproduction_environment(&self, spec: CommandSpec, run_root: &Path) -> CommandSpec {
        controlled_environment(spec, &run_root.join("caches"))
    }
    fn check_environment_root(&self, checkout: &Path) -> Result<(), String> {
        dependencies::check_checkout(checkout)
    }
    fn create_environment(&self, env_root: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(env_root.join("caches"))
    }
    fn check_environment(&self, env_root: &Path) -> Result<(), String> {
        dependencies::check(env_root)
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
                recorder_spec(worktree, "", "", config.as_deref())
                    .env("LYRA_VITEST_MODE", "suite")
                    .shown(suite_line(None, config.as_deref()))
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
