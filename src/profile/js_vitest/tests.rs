use super::*;
use std::collections::HashMap;

const FILE: &str = "tests/issue-7.test.ts";
const NAME: &str = "issue 7 > sums two numbers";

fn rec() -> ReproRecord {
    ReproRecord {
        reproduced: true,
        krate: String::new(),
        test_file: FILE.into(),
        test_name: NAME.into(),
        command: regression_line(FILE, &name_segments(NAME).unwrap()),
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

/// A checkout given as a map of files; `dir/` asks whether a directory exists.
fn reader<'a>(map: &'a HashMap<&'a str, &'a str>) -> impl Fn(&str) -> Option<String> + 'a {
    move |p: &str| {
        if p.ends_with('/') {
            return map.keys().any(|k| k.starts_with(p)).then(String::new);
        }
        if map.keys().any(|k| k.starts_with(&format!("{p}/"))) {
            return Some(String::new());
        }
        map.get(p).map(|s| s.to_string())
    }
}

fn report(tests: Value, file_errors: Value, unhandled: u64) -> String {
    format!(
        " RUN  v3.2.4 /w\n × tests/issue-7.test.ts > issue 7 > sums two numbers\nAssertionError: sum must be three: expected -1 to be 3\n{RESULT_PREFIX}{}\n{END_MARKER}\n",
        json!({ "tests": tests, "fileErrors": file_errors, "unhandled": unhandled, "started": true })
    )
}

fn test(name: &str, mode: &str, state: Option<&str>) -> Value {
    json!({ "file": FILE, "name": name, "mode": mode, "state": state, "errors": [] })
}

fn output(exit: i32, text: &str) -> CommandOutput {
    CommandOutput { exit_code: Some(exit), stdout: text.into(), ..Default::default() }
}

#[test]
fn detection_needs_a_vitest_signal_next_to_package_json() {
    let dir = tempfile::tempdir().unwrap();
    assert!(detect(dir.path()).is_none(), "nothing");
    std::fs::write(dir.path().join("package.json"), r#"{"name":"x","devDependencies":{"jest":"29"}}"#).unwrap();
    assert!(detect(dir.path()).is_none(), "package.json without Vitest");
    std::fs::write(dir.path().join("package.json"), r#"{"name":"x","devDependencies":{"vitest":"3"}}"#).unwrap();
    assert_eq!(detect(dir.path()).unwrap().evidence, ["package.json"]);
    std::fs::write(dir.path().join("vite.config.ts"), "export default {}").unwrap();
    assert_eq!(detect(dir.path()).unwrap().evidence, ["package.json", "vite.config.ts"]);
    std::fs::write(dir.path().join("vitest.config.mts"), "export default {}").unwrap();
    assert_eq!(detect(dir.path()).unwrap().evidence, ["package.json", "vitest.config.mts"]);
    std::fs::write(dir.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
    assert_eq!(detect(dir.path()).unwrap().evidence[1], "vitest.config.mts", "a Vitest config alone is evidence");
    std::fs::write(dir.path().join("package.json"), "not json").unwrap();
    assert!(detect(dir.path()).is_none(), "an unreadable package.json is no evidence");
}

#[test]
fn test_identity_is_validated_and_selected_exactly() {
    assert!(validate_test_file("tests/issue-7.test.ts").is_ok());
    assert!(validate_test_file("src/math.spec.js").is_ok());
    for bad in
        ["tests/issue-7.ts", "../x.test.ts", "/abs/x.test.ts", "node_modules/x/a.test.js", "tests/issue-7.test.json"]
    {
        assert!(validate_test_file(bad).is_err(), "{bad}");
    }
    assert_eq!(name_segments("issue 7 > sums two numbers").unwrap(), ["issue 7", "sums two numbers"]);
    for bad in ["", "a >  > b", "a\nb", &"x".repeat(501)] {
        assert!(name_segments(bad).is_err(), "{bad:?}");
    }
    let segments = name_segments("issue (7) > adds $1.50 + [tax]?").unwrap();
    let pattern = name_pattern(&segments);
    assert_eq!(pattern, r"^issue \(7\) adds \$1\.50 \+ \[tax\]\?$");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(re.is_match("issue (7) adds $1.50 + [tax]?") && !re.is_match("issue (7) adds $1.50 + [tax]? twice"));
    let line = regression_line(FILE, &name_segments(NAME).unwrap());
    assert_eq!(line, r#"npx vitest run tests/issue-7.test.ts -t "^issue 7 sums two numbers$""#);
    assert!(!line.contains('\n'));
    let mut record = rec();
    record.registration_file = Some("x".into());
    assert!(JsVitest.validate_record(&record).is_err(), "no registration for Vitest");
    let spec = JsVitest.regression_command(&rec(), Path::new("/w")).unwrap();
    assert_eq!(spec.program, NODE);
    assert_eq!(spec.args[..3], ["--input-type=module".to_string(), "-e".into(), RECORDER.into()]);
    assert_eq!(spec.args[3..], [FILE.to_string(), r"^issue 7 sums two numbers$".into()]);
    assert_eq!(spec.display(), line, "the shown command is the copyable line");
}

#[test]
fn classification_covers_every_outcome() {
    let r = rec();
    let ours = |state: Option<&str>, mode: &str| report(json!([test(NAME, mode, state)]), json!([]), 0);
    assert_eq!(classify_report(&ours(Some("fail"), "run"), &r), RegressionOutcome::Failed);
    assert_eq!(classify_report(&ours(Some("pass"), "run"), &r), RegressionOutcome::Passed);
    assert_eq!(classify_report(&ours(None, "skip"), &r), RegressionOutcome::Skipped);
    assert_eq!(classify_report(&ours(None, "todo"), &r), RegressionOutcome::Skipped);
    assert_eq!(classify_report(&ours(Some("skip"), "run"), &r), RegressionOutcome::Skipped);
    assert_eq!(
        classify_report(&report(json!([test("other", "run", Some("fail"))]), json!([]), 0), &r),
        RegressionOutcome::NotDiscovered
    );
    let broken = report(json!([]), json!([{ "file": FILE, "errors": ["SyntaxError: Unexpected token"] }]), 0);
    assert_eq!(classify_report(&broken, &r), RegressionOutcome::SetupFailed);
    assert_eq!(
        classify_report(&report(json!([test(NAME, "run", Some("fail"))]), json!([]), 1), &r),
        RegressionOutcome::SetupFailed
    );
    let truncated = ours(Some("fail"), "run").replace(END_MARKER, "");
    assert!(matches!(classify_report(&truncated, &r), RegressionOutcome::Other(_)), "no end marker");
    assert!(matches!(classify_report("garbled", &r), RegressionOutcome::Other(_)));
    let not_started = format!(
        "{RESULT_PREFIX}{}\n{END_MARKER}\n",
        json!({"tests": [], "fileErrors": [], "unhandled": 0, "started": false})
    );
    assert_eq!(classify_report(&not_started, &r), RegressionOutcome::SetupFailed);
    // CRLF output parses the same.
    assert_eq!(classify_report(&ours(Some("fail"), "run").replace('\n', "\r\n"), &r), RegressionOutcome::Failed);
}

#[test]
fn confirmation_and_pass_require_exactly_the_selected_test() {
    let r = rec();
    let failed = report(json!([test(NAME, "run", Some("fail")), test("issue 7 > other", "skip", None)]), json!([]), 0);
    assert!(confirm_failure_in(&output(1, &failed), &r).is_ok());
    assert!(confirm_failure_in(&output(0, &failed), &r).is_err(), "exit status 1 required");
    let two =
        report(json!([test(NAME, "run", Some("fail")), test("issue 7 > other", "run", Some("pass"))]), json!([]), 0);
    assert!(confirm_failure_in(&output(1, &two), &r).is_err(), "only one test may execute");
    let other_file = report(
        json!([{ "file": "tests/x.test.ts", "name": NAME, "mode": "run", "state": "fail", "errors": [] }]),
        json!([]),
        0,
    );
    assert!(confirm_failure_in(&output(1, &other_file), &r).is_err(), "same name in another file");
    let with_file_error =
        report(json!([test(NAME, "run", Some("fail"))]), json!([{ "file": "tests/y.test.ts", "errors": ["boom"] }]), 0);
    assert!(confirm_failure_in(&output(1, &with_file_error), &r).is_err());
    let passed = report(json!([test(NAME, "run", Some("pass"))]), json!([]), 0);
    assert!(assert_passed_in(&passed, &r).is_ok());
    assert!(assert_passed_in(&failed, &r).is_err());
    assert!(assert_passed_in(&report(json!([test(NAME, "skip", None)]), json!([]), 0), &r).is_err());
}

#[test]
fn source_binding_requires_no_test_from_the_emptied_file() {
    let r = rec();
    let empty = report(json!([]), json!([{ "file": FILE, "errors": ["No test suite found in file"] }]), 0);
    assert!(assert_absent(&empty, &r).is_ok());
    let still = report(json!([test("issue 7 > injected", "run", Some("pass"))]), json!([]), 0);
    assert!(assert_absent(&still, &r).is_err(), "a test still comes from the file");
    assert!(assert_absent("", &r).is_err(), "no report is not evidence");
    let spec = JsVitest.regression_command(&r, Path::new("/w")).unwrap();
    let discovery = JsVitest.discovery(&spec);
    assert_eq!(discovery.spec.args[3..], [FILE.to_string(), String::new()], "the whole file, no name filter");
    assert!(discovery.spec.env.iter().any(|(k, v)| k == "LYRA_VITEST_MODE" && v == "discover"));
}

#[test]
fn production_code_is_what_the_repository_declares_and_exists() {
    let files = HashMap::from([
        ("package.json", r#"{"main":"dist/index.js","module":"./lib/index.mjs","bin":{"calc":"./bin/calc.js"},"exports":{".":{"import":"./dist/index.mjs"},"./util":"./util/index.js"},"files":["dist","src","README.md","*.d.ts"]}"#),
        ("tsconfig.json", "{\n  // TypeScript sources\n  \"compilerOptions\": { \"rootDir\": \"src\", /* emitted */ \"outDir\": \"dist\" },\n  \"include\": [\"src/**/*\", \"types\",],\n}\n"),
        ("lyra.toml", "[profiles.js-vitest]\nsources = [\"extra\", \"ghost\"]\n"),
        ("dist/index.js", ""),
        ("lib/index.mjs", ""),
        ("bin/calc.js", ""),
        ("util/index.js", ""),
        ("src/calc.ts", ""),
        ("types/x.d.ts", ""),
        ("extra/a.js", ""),
        ("README.md", ""),
    ]);
    let production = ProductionCode::declared(&reader(&files));
    let roots: Vec<&str> = production.roots.iter().map(String::as_str).collect();
    assert_eq!(roots, ["README.md", "bin", "dist", "extra", "lib", "src", "types", "util"]);
    assert_eq!(production.missing, BTreeSet::from(["ghost".to_string()]));
    // `dist` named only by entry points is build output; `files` names it explicitly here.
    let entry_only = HashMap::from([("package.json", r#"{"main":"dist/index.js"}"#), ("dist/index.js", "")]);
    assert!(ProductionCode::declared(&reader(&entry_only)).is_empty(), "build output named by main is not source");
    for (path, want) in [
        ("src/calc.ts", Classification::Production),
        ("src/calc.test.ts", Classification::TestSupport),
        ("src/__tests__/calc.ts", Classification::TestSupport),
        ("src/__mocks__/fs.ts", Classification::TestSupport),
        ("src/__snapshots__/calc.test.ts.snap", Classification::TestSupport),
        ("src/setupTests.ts", Classification::TestSupport),
        ("support/helpers.ts", Classification::TestSupport),
        ("tests/helpers.ts", Classification::TestSupport),
    ] {
        assert_eq!(production.classify(path), want, "{path}");
    }
    let none = ProductionCode::declared(&reader(&HashMap::from([("package.json", "{}")])));
    assert_eq!(none.classify("src/calc.ts"), Classification::Unknown);
    assert_eq!(none.classify("data/x.json"), Classification::TestSupport, "data is frozen either way");
    let message = production.unknown_message(&BTreeSet::from(["x.ts".to_string()]));
    assert!(message.contains("`ghost`") && message.contains("lyra.toml"), "{message}");
    assert_eq!(top_component("./src/**/*.ts").as_deref(), Some("src"));
    assert_eq!(top_component("*.d.ts"), None, "wildcard-only declares nothing");
    assert_eq!(top_component("../outside"), None);
}

#[test]
fn loaded_modules_are_frozen_unless_production_and_foreign_code_fails_closed() {
    let files = HashMap::from([("package.json", r#"{"files":["src"]}"#), ("src/calc.ts", "")]);
    let production = ProductionCode::declared(&reader(&files));
    let out = format!(
        "{RESULT_PREFIX}{{}}\n{LOADED_PREFIX}src/calc.ts\n{LOADED_PREFIX}{FILE}\n{LOADED_PREFIX}tests/helpers.ts\n{LOADED_PREFIX}support/data.ts\n{LOADED_PREFIX}vitest.setup.ts\n{END_MARKER}\n"
    );
    assert_eq!(
        loaded_inputs(&out, FILE, &production).unwrap(),
        ["support/data.ts", "tests/helpers.ts", "vitest.setup.ts"]
    );
    let err = loaded_inputs(&out.replace(END_MARKER, ""), FILE, &production).unwrap_err();
    assert!(err.contains("cannot establish"), "{err}");
    let foreign = out.replace(END_MARKER, &format!("{FOREIGN_PREFIX}/home/op/repo/src/calc.ts\n{END_MARKER}"));
    let err = loaded_inputs(&foreign, FILE, &production).unwrap_err();
    assert!(err.contains("`/home/op/repo/src/calc.ts`") && err.contains("instead of the checkout"), "{err}");
    let err = loaded_inputs(&out, FILE, &ProductionCode::default()).unwrap_err();
    assert!(err.contains("`src/calc.ts`") && err.contains("declares no source directories"), "{err}");
}

#[test]
fn repro_may_add_only_the_test_and_non_code_fixtures() {
    let ok = vec![("??".to_string(), FILE.to_string()), ("??".to_string(), "tests/fixtures/frame.json".to_string())];
    assert!(check_repro_changes_with(&ok, &rec()).is_ok());
    for (status, path) in [
        ("??", "tests/helpers.ts"),
        ("??", "tests/fixtures/load.ts"),
        ("??", "vitest.setup.ts"),
        ("M", "package.json"),
        ("??", "node_modules/x/index.js"),
        ("M", "src/calc.ts"),
        ("??", "tests/__snapshots__/issue-7.test.ts.snap"),
        ("M", "vitest.config.ts"),
        ("??", ".npmrc"),
    ] {
        let changes = vec![("??".to_string(), FILE.to_string()), (status.to_string(), path.to_string())];
        assert!(check_repro_changes_with(&changes, &rec()).is_err(), "{status} {path}");
    }
    assert!(check_repro_changes_with(&[("M".into(), FILE.into())], &rec()).is_err(), "test must be new");
    assert!(check_repro_changes_with(&[], &rec()).is_err(), "test must exist");
}

#[test]
fn fix_may_change_only_declared_source_code() {
    let checkpoint = HashMap::from([
        (FILE, "import { add } from '../src/calc'\n"),
        ("package.json", r#"{"name":"calc","files":["src"],"devDependencies":{"vitest":"3"}}"#),
        ("src/calc.ts", "export const add = (a, b) => a - b\n"),
        ("src/data.json", "{}\n"),
        ("src/__tests__/old.test.ts", ""),
        ("tests/helpers.ts", ""),
        ("README.md", ""),
    ]);
    let read_checkpoint = reader(&checkpoint);
    let check = |changes: &[(&str, &str)]| {
        let mut all = vec![("M".to_string(), "src/calc.ts".to_string())];
        all.extend(changes.iter().map(|(s, p)| (s.to_string(), p.to_string())));
        check_fix_changes_with(&all, &rec(), &read_checkpoint, &read_checkpoint)
    };
    assert!(check(&[]).is_ok());
    for ok in [vec![("A", "src/util.ts")], vec![("D", "src/util.ts")], vec![("D", "src/calc.ts"), ("A", "src/sum.ts")]]
    {
        assert!(check(&ok).is_ok(), "{ok:?}");
    }
    for bad in [
        ("M", "package.json"),
        ("M", "package-lock.json"),
        ("A", "pnpm-lock.yaml"),
        ("M", "tsconfig.json"),
        ("M", "tsconfig.build.json"),
        ("A", "vitest.config.ts"),
        ("M", "src/vite.config.ts"),
        ("A", "eslint.config.js"),
        ("A", ".npmrc"),
        ("A", "node_modules/vitest/index.js"),
        ("A", "src/node_modules/calc/index.js"),
        ("M", "src/__tests__/old.test.ts"),
        ("A", "src/calc.spec.ts"),
        ("A", "src/__mocks__/calc.ts"),
        ("A", "src/__snapshots__/x.snap"),
        ("A", "src/setupTests.ts"),
        ("M", "tests/helpers.ts"),
        ("M", "src/data.json"),
        ("M", "README.md"),
        ("A", "lib/new.ts"),
        ("M", FILE),
    ] {
        let err = check(&[bad]).unwrap_err();
        assert!(err.contains(bad.1), "{bad:?}: {err}");
    }
    // A frozen input takes precedence even inside the declared sources.
    let mut record = rec();
    record.frozen_inputs = vec!["src/calc.ts".into()];
    let mut now = checkpoint.clone();
    now.insert("src/calc.ts", "export const add = (a, b) => a + b\n");
    let err = check_fix_changes_with(&[("M".into(), "src/calc.ts".into())], &record, &read_checkpoint, &reader(&now))
        .unwrap_err();
    assert!(err.contains("src/calc.ts") && err.contains("locked"), "{err}");
    // The recorded command is the profile's own.
    let mut changed = rec();
    changed.command = "npx vitest run".into();
    assert!(check_fix_changes_with(
        &[("M".into(), "src/calc.ts".into())],
        &changed,
        &read_checkpoint,
        &read_checkpoint
    )
    .is_err());
    assert!(check_fix_changes_with(&[], &rec(), &read_checkpoint, &read_checkpoint).is_err(), "no changes");
}

#[test]
fn environment_is_scrubbed_and_caches_stay_in_the_run() {
    let spec = CommandSpec::new(NODE, "/w").env("NODE_OPTIONS", "--require /evil.js").env("NODE_PATH", "/x");
    let spec = JsVitest.command_environment(spec, Path::new("/run"));
    assert!(spec.clear_env);
    assert!(!spec.env.iter().any(|(k, _)| k == "NODE_OPTIONS" || k == "NODE_PATH"));
    let get = |k: &str| spec.env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    assert_eq!(get("CI").as_deref(), Some("1"));
    assert_eq!(
        get("LYRA_VITEST_CACHE"),
        Some(Path::new("/run").join("caches").join("vite").to_string_lossy().into_owned())
    );
}

#[test]
fn verification_runs_the_regression_and_the_suite_through_the_recorder() {
    let steps = JsVitest.verification_steps(&VerifyCommands::default(), &rec(), Path::new("/w")).unwrap();
    assert_eq!(steps.iter().map(|s| s.name).collect::<Vec<_>>(), ["repro", "tests"]);
    assert!(steps[0].regression);
    assert_eq!(steps[1].spec.args[3..], [String::new(), String::new()], "the whole suite");
    assert_eq!(steps[1].spec.display(), "npx vitest run");
    let configured = VerifyCommands {
        build: Some("npx tsc --noEmit".into()),
        test: Some("npm test".into()),
        fmt: Some("npx prettier --check .".into()),
    };
    let steps = JsVitest.verification_steps(&configured, &rec(), Path::new("/w")).unwrap();
    assert_eq!(steps.iter().map(|s| s.name).collect::<Vec<_>>(), ["build", "repro", "tests", "fmt"]);
    assert_eq!(steps[2].spec.display(), "npm test");
}

#[cfg(unix)]
#[test]
fn node_modules_is_linked_above_the_checkout_once() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("node_modules/vitest")).unwrap();
    std::fs::write(repo.join("package.json"), "{}").unwrap();
    let run = dir.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    link_node_modules(&repo, &run).unwrap();
    assert!(run.join("node_modules/vitest").is_dir());
    link_node_modules(&repo, &run).unwrap();
    assert!(JsVitest.check_environment(&run).is_ok());
    // Not installed: an actionable error.
    let bare = dir.path().join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    let err = link_node_modules(&bare, &dir.path().join("run2")).unwrap_err();
    assert!(err.contains("npm ci") && err.contains("no installed Vitest"), "{err}");
    // Workspaces are not supported yet, visibly.
    std::fs::write(repo.join("package.json"), r#"{"workspaces":["packages/*"]}"#).unwrap();
    let err = link_node_modules(&repo, &dir.path().join("run3")).unwrap_err();
    assert!(err.contains("workspace"), "{err}");
}

#[test]
fn profile_surface() {
    let p = JsVitest;
    assert_eq!((p.id(), p.language()), (ID, "JavaScript/TypeScript"));
    assert_eq!(p.required_tools(), [NODE]);
    assert!(p.is_test_path("src/a.test.ts") && p.is_test_path("tests/x.ts") && !p.is_test_path("src/a.ts"));
    assert!(p.is_source_file(Path::new("src/a.tsx")) && p.is_source_file(Path::new("package.json")));
    assert!(p.generated_dirs().contains(&"node_modules"));
    assert!(p
        .explain_failure("LYRA-STARTUP-ERROR Error: Cannot find package 'vitest'\n    at x", Some(2))
        .contains("did not start"));
    assert!(p.preparation(Path::new("/w"), &|_| false).is_none());
}

// ---------------------------------------------------------------------------------------
// Real Vitest: `LYRA_TEST_NODE_MODULES` names a node_modules with vitest installed.
// ---------------------------------------------------------------------------------------

#[cfg(unix)]
mod provisioned {
    use super::*;
    use std::path::PathBuf;

    const IGNORE: &str = "needs node and LYRA_TEST_NODE_MODULES pointing at a node_modules with vitest";

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        run: PathBuf,
        checkout: PathBuf,
    }

    impl Fixture {
        /// An operator repository whose `node_modules` holds the provisioned packages, and a
        /// LYRA run directory with the link above its checkout.
        fn new(files: &[(&str, &str)]) -> Self {
            let provisioned = PathBuf::from(std::env::var("LYRA_TEST_NODE_MODULES").expect(IGNORE));
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("repo");
            std::fs::create_dir_all(repo.join("node_modules")).unwrap();
            for entry in std::fs::read_dir(&provisioned).unwrap().flatten() {
                std::os::unix::fs::symlink(entry.path(), repo.join("node_modules").join(entry.file_name())).unwrap();
            }
            let run = dir.path().join("run");
            let checkout = run.join("checkout");
            for (rel, text) in files {
                for base in [&repo, &checkout] {
                    let path = base.join(rel);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, text).unwrap();
                }
            }
            link_node_modules(&repo, &run).unwrap();
            Fixture { _dir: dir, repo, run, checkout }
        }
        fn write(&self, rel: &str, text: &str) {
            let path = self.checkout.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        async fn run(&self, spec: CommandSpec) -> CommandOutput {
            let spec = JsVitest.reproduction_environment(spec, &self.run);
            proc::run(&spec, Duration::from_secs(180), &CancellationToken::new()).await.unwrap()
        }
        fn snapshot(&self, paths: &[&str]) -> HashMap<String, String> {
            paths.iter().map(|p| (p.to_string(), std::fs::read_to_string(self.checkout.join(p)).unwrap())).collect()
        }
    }

    const PACKAGE: &str = r#"{"name":"calc","type":"module","files":["src"],"devDependencies":{"vitest":"3"}}"#;
    const TEST: &str = "import { describe, it, expect } from 'vitest'\nimport { add } from '../src/calc.js'\ndescribe('issue 7', () => {\n  it('sums two numbers', () => { expect(add(1, 2), 'sum must be three').toBe(3) })\n  it('other', () => { expect(1).toBe(1) })\n})\n";

    fn record() -> ReproRecord {
        let mut r = rec();
        r.test_file = "tests/issue-7.test.js".into();
        r.command = regression_line("tests/issue-7.test.js", &name_segments(NAME).unwrap());
        r
    }

    #[tokio::test]
    #[ignore = "needs node and LYRA_TEST_NODE_MODULES pointing at a node_modules with vitest"]
    async fn real_vitest_failure_binding_and_pass_after_fix() {
        let fx = Fixture::new(&[
            ("package.json", PACKAGE),
            ("src/calc.js", "export function add(a, b) { return a - b }\n"),
            (
                "tests/existing.test.js",
                "import { it, expect } from 'vitest'\nit('existing', () => expect(1).toBe(1))\n",
            ),
            ("tests/issue-7.test.js", TEST),
        ]);
        let r = record();
        let spec = JsVitest.regression_command(&r, &fx.checkout).unwrap();
        let run = fx.run(spec.clone()).await;
        let out = run.combined();
        assert_eq!(classify_report(&out, &r), RegressionOutcome::Failed, "{out}");
        confirm_failure_in(&run, &r).unwrap();
        assert!(guards::output_contains_expected(&out, &r.expected), "{out}");
        assert_eq!(JsVitest.loaded_test_inputs(&out, &r, &fx.checkout).unwrap(), Vec::<String>::new(), "{out}");
        assert!(
            !fx.repo.join("node_modules/.vite").exists() && !fx.run.join("node_modules/.vite").exists(),
            "no cache in the shared node_modules"
        );

        // Source binding: with the test file emptied, nothing is found in it.
        fx.write("tests/issue-7.test.js", "");
        let discovery = JsVitest.discovery(&JsVitest.reproduction_environment(spec.clone(), &fx.run));
        let found = proc::run(&discovery.spec, Duration::from_secs(180), &CancellationToken::new()).await.unwrap();
        assert!(found.success(), "{}", found.combined());
        assert_absent(&found.stdout, &r).unwrap();
        fx.write("tests/issue-7.test.js", TEST);

        // The fix passes the regression and the suite.
        fx.write("src/calc.js", "export function add(a, b) { return a + b }\n");
        let out = fx.run(spec).await.combined();
        assert_passed_in(&out, &r).unwrap();
        let steps = JsVitest.verification_steps(&VerifyCommands::default(), &r, &fx.checkout).unwrap();
        let suite = fx.run(steps[1].spec.clone()).await;
        assert!(suite.success(), "{}", suite.combined());
    }

    #[tokio::test]
    #[ignore = "needs node and LYRA_TEST_NODE_MODULES pointing at a node_modules with vitest"]
    async fn a_helper_the_regression_loads_is_frozen_and_editing_it_is_rejected() {
        let test = "import { describe, it, expect } from 'vitest'\nimport { add } from '../src/calc.js'\nimport { EXPECTED } from './helpers.js'\ndescribe('issue 7', () => {\n  it('sums two numbers', () => { expect(add(1, 2), 'sum must be three').toBe(EXPECTED) })\n})\n";
        let fx = Fixture::new(&[
            ("package.json", PACKAGE),
            ("src/calc.js", "export function add(a, b) { return a - b }\n"),
            ("tests/helpers.js", "export const EXPECTED = 3\n"),
            ("tests/issue-7.test.js", test),
        ]);
        let r = record();
        let spec = JsVitest.regression_command(&r, &fx.checkout).unwrap();
        let out = fx.run(spec.clone()).await.combined();
        assert_eq!(classify_report(&out, &r), RegressionOutcome::Failed, "{out}");
        let loaded = JsVitest.loaded_test_inputs(&out, &r, &fx.checkout).unwrap();
        assert_eq!(loaded, ["tests/helpers.js"], "{out}");
        let frozen = fx.snapshot(&["package.json", "src/calc.js", "tests/helpers.js", "tests/issue-7.test.js"]);
        fx.write("tests/helpers.js", "export const EXPECTED = -1\n");
        let out = fx.run(spec).await.combined();
        assert_eq!(classify_report(&out, &r), RegressionOutcome::Passed, "the attack works without the lock: {out}");
        let read_checkpoint = |p: &str| frozen.get(p).cloned();
        let read_now = |p: &str| std::fs::read_to_string(fx.checkout.join(p)).ok();
        let change = vec![("M".to_string(), "tests/helpers.js".to_string())];
        for record in [&ReproRecord { frozen_inputs: loaded, ..r.clone() }, &r] {
            let err = check_fix_changes_with(&change, record, &read_checkpoint, &read_now).unwrap_err();
            assert!(err.contains("tests/helpers.js"), "{err}");
        }
    }

    #[tokio::test]
    #[ignore = "needs node and LYRA_TEST_NODE_MODULES pointing at a node_modules with vitest"]
    async fn code_resolved_from_the_operators_clone_fails_closed() {
        // A package link in the operator's node_modules points back into the operator's clone
        // (as a workspace link would): there the code is fixed, in the checkout it is buggy.
        let fx = Fixture::new(&[
            ("package.json", PACKAGE),
            ("src/calc.js", "export function add(a, b) { return a - b }\n"),
            ("tests/issue-7.test.js", &TEST.replace("'../src/calc.js'", "'calc-self/calc.js'")),
        ]);
        std::fs::write(fx.repo.join("src/calc.js"), "export function add(a, b) { return a + b }\n").unwrap();
        std::fs::write(fx.repo.join("src/package.json"), r#"{"name":"calc-self","type":"module"}"#).unwrap();
        std::os::unix::fs::symlink(fx.repo.join("src"), fx.repo.join("node_modules/calc-self")).unwrap();
        let r = record();
        let out = fx.run(JsVitest.regression_command(&r, &fx.checkout).unwrap()).await.combined();
        assert_eq!(classify_report(&out, &r), RegressionOutcome::Passed, "precondition: the clone's code ran: {out}");
        let err = JsVitest.loaded_test_inputs(&out, &r, &fx.checkout).unwrap_err();
        assert!(err.contains("instead of the checkout") && err.contains("src/calc.js"), "{err}");
    }
}
