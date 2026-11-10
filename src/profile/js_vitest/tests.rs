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
        command: regression_line(FILE, NAME, None),
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

/// A recorder report: `tests`, `fileErrors`, `unhandled`, with the test file loaded and the
/// module graph observed.
fn report(tests: Value, file_errors: Value, unhandled: u64) -> String {
    report_with(json!({ "tests": tests, "fileErrors": file_errors, "unhandled": unhandled }), &[FILE], &[])
}

/// A complete report from `fields` (defaults filled in) and module records.
fn report_with(fields: Value, loaded: &[&str], foreign: &[&str]) -> String {
    let mut value = json!({
        "started": true, "graph": true, "tests": [], "fileErrors": [], "suiteErrors": [], "unhandled": 0, "setup": []
    });
    for (k, v) in fields.as_object().unwrap() {
        value[k] = v.clone();
    }
    let mut out = format!(
        " RUN  v3.2.4 /w\n × tests/issue-7.test.ts > issue 7 > sums two numbers\nAssertionError: sum must be three\n{RESULT_PREFIX}{value}\n"
    );
    for path in loaded {
        out.push_str(&format!("{LOADED_PREFIX}{path}\n"));
    }
    for path in foreign {
        out.push_str(&format!("{FOREIGN_PREFIX}{path}\n"));
    }
    out.push_str(END_MARKER);
    out.push('\n');
    out
}

fn test(name: &str, mode: &str, state: Option<&str>) -> Value {
    let errors = if state == Some("fail") { json!(["sum must be three: expected -1 to be 3"]) } else { json!([]) };
    json!({ "file": FILE, "name": name, "mode": mode, "state": state, "errors": errors })
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
    assert!(validate_test_file("--sample.test.js").is_ok(), "a dash-led name is a valid path");
    for bad in [
        "tests/issue-7.ts",
        "../x.test.ts",
        "/abs/x.test.ts",
        "node_modules/x/a.test.js",
        "src/Node_Modules/x/a.test.js",
        "tests/issue-7.test.json",
    ] {
        assert!(validate_test_file(bad).is_err(), "{bad}");
    }
    assert_eq!(name_segments("issue 7 > sums two numbers").unwrap(), ["issue 7", "sums two numbers"]);
    for bad in ["", "a >  > b", "a\nb", &"x".repeat(501)] {
        assert!(name_segments(bad).is_err(), "{bad:?}");
    }
    let pattern = name_pattern("issue (7) > adds $1.50 + [tax]?");
    assert_eq!(pattern, r"^issue \(7\)(?: | > )adds \$1\.50 \+ \[tax\]\?$");
    let re = regex::Regex::new(&pattern).unwrap();
    assert!(re.is_match("issue (7) adds $1.50 + [tax]?") && !re.is_match("issue (7) adds $1.50 + [tax]? twice"));
    // A title that itself contains ` > ` keeps it: Vitest reports `issue 7 > rejects x > y` for
    // `describe('issue 7', () => it('rejects x > y'))` and matches `issue 7 rejects x > y`.
    let re = regex::Regex::new(&name_pattern("issue 7 > rejects x > y")).unwrap();
    assert!(re.is_match("issue 7 rejects x > y"), "the literal separator in the title survives");
    assert!(!re.is_match("issue 7 rejects x y z"));

    let line = regression_line(FILE, NAME, None);
    assert_eq!(line, r"npx vitest run tests/issue-7.test.ts -t '^issue 7(?: | > )sums two numbers$'");
    assert!(!line.contains('\n'));
    // Paths and patterns stay literal when pasted into a POSIX shell.
    let odd = regression_line("tests/a b/it's $(x).test.ts", "a's > b", Some("config/unit.ts"));
    assert_eq!(odd, r"npx vitest run 'tests/a b/it'\''s $(x).test.ts' -t '^a'\''s(?: | > )b$' --config config/unit.ts");
    let words = shell_words(&odd);
    assert_eq!(words[3], "tests/a b/it's $(x).test.ts");
    assert_eq!(words[5], "^a's(?: | > )b$");

    let mut record = rec();
    record.registration_file = Some("x".into());
    assert!(JsVitest.validate_record(&record).is_err(), "no registration for Vitest");
    let dir = tempfile::tempdir().unwrap();
    let spec = JsVitest.regression_command(&rec(), dir.path()).unwrap();
    assert_eq!(spec.program, NODE);
    assert_eq!(spec.args[..4], ["--input-type=module".to_string(), "-e".into(), RECORDER.into(), "--".into()]);
    assert_eq!(spec.args[4..], [FILE.to_string(), name_pattern(NAME), String::new()]);
    assert_eq!(spec.display(), line, "the shown command is the copyable line");
    assert_eq!(JsVitest.regression_command_line(&rec(), dir.path()), line);
}

/// POSIX shell word splitting for the forms `sh_quote` produces: single quotes and `\\'`.
fn shell_words(line: &str) -> Vec<String> {
    let (mut words, mut word, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('\'', _) => {
                quoted = !quoted;
                any = true;
            }
            ('\\', false) => {
                word.extend(chars.next());
                any = true;
            }
            (' ', false) => {
                if any {
                    words.push(std::mem::take(&mut word));
                }
                any = false;
            }
            (c, _) => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(word);
    }
    words
}

#[test]
fn a_nondefault_vitest_config_is_declared_and_shared_or_refused() {
    let dir = tempfile::tempdir().unwrap();
    let write = |rel: &str, text: &str| {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write("package.json", r#"{"scripts":{"test":"vitest run --config config/unit.ts"}}"#);
    let err = JsVitest.regression_command(&rec(), dir.path()).unwrap_err();
    assert!(err.contains("config = ") && err.contains("lyra.toml"), "{err}");
    assert!(JsVitest.verification_steps(&VerifyCommands::default(), &rec(), dir.path()).is_err());
    write("lyra.toml", "[profiles.js-vitest]\nconfig = \"config/missing.ts\"\n");
    assert!(JsVitest.regression_command(&rec(), dir.path()).unwrap_err().contains("existing configuration file"));
    write("config/unit.ts", "export default {}\n");
    write("lyra.toml", "[profiles.js-vitest]\nconfig = \"config/unit.ts\"\n");
    let spec = JsVitest.regression_command(&rec(), dir.path()).unwrap();
    assert_eq!(spec.args.last().unwrap(), "config/unit.ts");
    assert!(spec.display().ends_with(" --config config/unit.ts"), "{}", spec.display());
    let discovery = JsVitest.discovery(&spec);
    assert_eq!(discovery.spec.args[4..], [FILE.to_string(), String::new(), "config/unit.ts".into()]);
    let steps = JsVitest.verification_steps(&VerifyCommands::default(), &rec(), dir.path()).unwrap();
    assert_eq!(steps[1].spec.args.last().unwrap(), "config/unit.ts");
    assert_eq!(steps[1].spec.display(), "npx vitest run --config config/unit.ts");
    for bad in ["/etc/x.ts", "../x.ts", "C:/x.ts"] {
        write("lyra.toml", &format!("[profiles.js-vitest]\nconfig = \"{bad}\"\n"));
        assert!(JsVitest.regression_command(&rec(), dir.path()).is_err(), "{bad}");
    }
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
    let hook = report_with(
        json!({ "tests": [test(NAME, "run", Some("fail"))], "suiteErrors": [{ "file": FILE, "name": "issue 7", "errors": ["afterAll broke"] }] }),
        &[FILE],
        &[],
    );
    assert_eq!(classify_report(&hook, &r), RegressionOutcome::SetupFailed, "a suite hook error");
    let truncated = ours(Some("fail"), "run").replace(END_MARKER, "");
    assert!(matches!(classify_report(&truncated, &r), RegressionOutcome::Other(_)), "no end marker");
    assert!(matches!(classify_report("garbled", &r), RegressionOutcome::Other(_)));
    let not_started = report_with(json!({ "started": false }), &[], &[]);
    assert_eq!(classify_report(&not_started, &r), RegressionOutcome::SetupFailed);
    // CRLF output parses the same.
    assert_eq!(classify_report(&ours(Some("fail"), "run").replace('\n', "\r\n"), &r), RegressionOutcome::Failed);
}

#[test]
fn a_result_printed_by_the_test_never_replaces_the_recorders() {
    let r = rec();
    let real = report(json!([test(NAME, "run", Some("fail"))]), json!([]), 0);
    let forged = report(json!([test(NAME, "run", Some("pass"))]), json!([]), 0);
    // A forged result (and end marker) on stderr, after the real one in the combined output.
    let run = CommandOutput { exit_code: Some(0), stdout: real.clone(), stderr: forged.clone(), ..Default::default() };
    assert!(parse_report(&run.combined()).is_none(), "two results are inconclusive");
    assert!(matches!(classify_report(&run.combined(), &r), RegressionOutcome::Other(_)));
    assert!(assert_passed_in(&run.combined(), &r).is_err());
    // Only the result line forged (no second end marker), before the real one.
    let line = forged.lines().find(|l| l.starts_with(RESULT_PREFIX)).unwrap();
    assert!(parse_report(&format!("{line}\n{real}")).is_none());
    // A stray end marker printed by the test before the real result.
    assert!(parse_report(&format!("{END_MARKER}\n{real}")).is_none());
    // Module records count only inside the recorder's block.
    let outside = format!("{LOADED_PREFIX}tests/sneaky.ts\n{FOREIGN_PREFIX}/elsewhere/x.js\n{real}");
    let parsed = parse_report(&outside).unwrap();
    assert_eq!((parsed.loaded, parsed.foreign), (vec![FILE.to_string()], Vec::<String>::new()));
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
    let hook = report_with(
        json!({ "tests": [test(NAME, "run", Some("fail"))], "suiteErrors": [{ "file": FILE, "name": "issue 7", "errors": ["afterAll broke"] }] }),
        &[FILE],
        &[],
    );
    let err = confirm_failure_in(&output(1, &hook), &r).unwrap_err();
    assert!(err.contains("suite hook"), "{err}");
    // The expected text must be in the selected test's own error, not just in the output.
    let unrelated = report(
        json!([{ "file": FILE, "name": NAME, "mode": "run", "state": "fail", "errors": ["TypeError: x is not a function"] }]),
        json!([]),
        0,
    );
    assert!(guards::output_contains_expected(&unrelated, &r.expected), "precondition: the logs contain it");
    let err = confirm_failure_in(&output(1, &unrelated), &r).unwrap_err();
    assert!(err.contains("own error"), "{err}");

    let passed = report(json!([test(NAME, "run", Some("pass"))]), json!([]), 0);
    assert!(assert_passed_in(&passed, &r).is_ok());
    assert!(assert_passed_in(&failed, &r).is_err());
    assert!(assert_passed_in(&report(json!([test(NAME, "skip", None)]), json!([]), 0), &r).is_err());
    // Verify rejects a passing report that used code from outside the checkout too.
    let fields = json!({ "tests": [test(NAME, "run", Some("pass"))] });
    let foreign = report_with(fields.clone(), &[FILE], &["/home/op/repo/src/calc.ts"]);
    let err = assert_passed_in(&foreign, &r).unwrap_err();
    assert!(err.contains("/home/op/repo/src/calc.ts"), "{err}");
    // ... and one whose module graph could not be observed.
    let blind = report_with(json!({ "tests": [test(NAME, "run", Some("pass"))], "graph": false }), &[FILE], &[]);
    assert!(assert_passed_in(&blind, &r).unwrap_err().contains("module graph"));
    let unloaded = report_with(fields, &[], &[]);
    assert!(assert_passed_in(&unloaded, &r).unwrap_err().contains("module graph"));
}

#[test]
fn source_binding_requires_an_empty_file_and_nothing_else() {
    let r = rec();
    let collected = |file_errors: Value, unhandled: u64| {
        report_with(json!({ "collected": [FILE], "fileErrors": file_errors, "unhandled": unhandled }), &[FILE], &[])
    };
    assert!(assert_absent(&collected(json!([]), 0), &r).is_ok());
    let empty = collected(json!([{ "file": FILE, "errors": ["No test suite found in file /w/x"] }]), 0);
    assert!(assert_absent(&empty, &r).is_ok(), "the empty-file error itself is expected");
    let still = report(json!([test("issue 7 > injected", "run", Some("pass"))]), json!([]), 0);
    assert!(assert_absent(&still, &r).is_err(), "a test still comes from the file");
    assert!(assert_absent("", &r).is_err(), "no report is not evidence");
    let not_collected = report(json!([]), json!([]), 0);
    assert!(assert_absent(&not_collected, &r).unwrap_err().contains("did not collect"));
    for broken in [
        collected(json!([{ "file": FILE, "errors": ["Error: setup exploded"] }]), 0),
        collected(json!([{ "file": "tests/other.test.ts", "errors": ["x"] }]), 0),
        collected(json!([]), 1),
    ] {
        assert!(assert_absent(&broken, &r).unwrap_err().contains("failed instead of finding no test"), "{broken}");
    }
    let dir = tempfile::tempdir().unwrap();
    let spec = JsVitest.regression_command(&r, dir.path()).unwrap();
    let discovery = JsVitest.discovery(&spec);
    assert_eq!(discovery.spec.args[4..], [FILE.to_string(), String::new(), String::new()], "the whole file");
    assert!(discovery.spec.env.iter().any(|(k, v)| k == "LYRA_VITEST_MODE" && v == "discover"));
    assert_eq!(discovery.spec.display(), format!("npx vitest run {FILE}"));
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
    assert_eq!(declared_path("./src/**/*.ts").as_deref(), Some("src"));
    assert_eq!(declared_path("src/app/**/*.ts").as_deref(), Some("src/app"));
    assert_eq!(declared_path("*.d.ts"), None, "wildcard-only declares nothing");
    assert_eq!(declared_path("../outside"), None);
}

#[test]
fn a_declaration_authorizes_only_its_own_scope() {
    let files = HashMap::from([
        ("package.json", r#"{"main":"./src/lib/index.js","bin":"cli.js"}"#),
        ("lyra.toml", "[profiles.js-vitest]\nsources = [\"src/app\", \"src/ghost\", \"src/app/**/*.ts\"]\n"),
        ("src/lib/index.js", ""),
        ("src/app/main.ts", ""),
        ("src/support/helper.js", ""),
        ("cli.js", ""),
        ("other.js", ""),
    ]);
    let production = ProductionCode::declared(&reader(&files));
    let roots: Vec<&str> = production.roots.iter().map(String::as_str).collect();
    assert_eq!(roots, ["cli.js", "src/app", "src/lib"], "an entry point declares its directory, a root entry itself");
    assert_eq!(production.missing, BTreeSet::from(["src/ghost".to_string()]), "a missing entry grants nothing");
    for (path, want) in [
        ("src/app/main.ts", Classification::Production),
        ("src/app/deep/x.ts", Classification::Production),
        ("src/lib/index.js", Classification::Production),
        ("cli.js", Classification::Production),
        ("src/support/helper.js", Classification::TestSupport),
        ("src/application.ts", Classification::TestSupport),
        ("other.js", Classification::TestSupport),
    ] {
        assert_eq!(production.classify(path), want, "{path}");
    }
    // The fix lock follows: a sibling helper under the same top-level directory is frozen.
    let changes = vec![("M".to_string(), "src/support/helper.js".to_string())];
    let err = check_fix_changes_with(&changes, &rec(), &reader(&files), &reader(&files)).unwrap_err();
    assert!(err.contains("src/support/helper.js") && err.contains("`src/app`"), "{err}");
}

#[test]
fn jsonc_keeps_quoted_text_and_drops_real_trailing_commas() {
    let text = "{\n  // c\n  \"compilerOptions\": { \"rootDir\": \"src,}\", },\n  \"include\": [\"a,]\", \"b\",],\n}";
    let value: Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
    assert_eq!(value["compilerOptions"]["rootDir"], "src,}");
    assert_eq!(value["include"], json!(["a,]", "b"]));
    assert_eq!(strip_jsonc(r#"{"a": "x\",}", "b": [1, /* , */ 2 ,  ]}"#), r#"{"a": "x\",}", "b": [1,  2   ]}"#);
}

#[test]
fn loaded_modules_and_configured_setup_are_frozen_and_foreign_code_fails_closed() {
    let files = HashMap::from([("package.json", r#"{"files":["src"]}"#), ("src/calc.ts", ""), ("src/init.js", "")]);
    let production = ProductionCode::declared(&reader(&files));
    let loaded = [FILE, "src/calc.ts", "tests/helpers.ts", "support/data.ts", "vitest.setup.ts", "src/init.js"];
    let fields = json!({ "setup": ["src/init.js", "config/global.js"] });
    let out = report_with(fields.clone(), &loaded, &[]);
    assert_eq!(
        loaded_inputs(&out, FILE, &production).unwrap(),
        ["config/global.js", "src/init.js", "support/data.ts", "tests/helpers.ts", "vitest.setup.ts"],
        "a configured setup file is frozen by path, whatever its name and also inside production code"
    );
    let err = loaded_inputs(&out.replace(END_MARKER, ""), FILE, &production).unwrap_err();
    assert!(err.contains("cannot establish"), "{err}");
    let foreign = report_with(fields.clone(), &loaded, &["/home/op/repo/src/calc.ts"]);
    let err = loaded_inputs(&foreign, FILE, &production).unwrap_err();
    assert!(err.contains("`/home/op/repo/src/calc.ts`") && err.contains("outside the checkout"), "{err}");
    let err = loaded_inputs(&out, FILE, &ProductionCode::default()).unwrap_err();
    assert!(err.contains("`src/calc.ts`") && err.contains("declares no source directories"), "{err}");
    let blind = report_with(json!({ "graph": false }), &loaded, &[]);
    assert!(loaded_inputs(&blind, FILE, &production).unwrap_err().contains("module graph"));
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
        ("A", "src/Node_Modules/calc/index.js"),
        ("A", "src/NODE_MODULES/calc/package.json"),
        ("A", "src/Tests/helper.ts"),
        ("A", "src/calc.Test.ts"),
        ("M", "Package.json"),
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
    let dir = tempfile::tempdir().unwrap();
    let steps = JsVitest.verification_steps(&VerifyCommands::default(), &rec(), dir.path()).unwrap();
    assert_eq!(steps.iter().map(|s| s.name).collect::<Vec<_>>(), ["repro", "tests"]);
    assert!(steps[0].regression);
    assert_eq!(steps[1].spec.args[4..], [String::new(), String::new(), String::new()], "the whole suite");
    assert_eq!(steps[1].spec.display(), "npx vitest run");
    let configured = VerifyCommands {
        build: Some("npx tsc --noEmit".into()),
        test: Some("npm test".into()),
        fmt: Some("npx prettier --check .".into()),
    };
    let steps = JsVitest.verification_steps(&configured, &rec(), dir.path()).unwrap();
    assert_eq!(steps.iter().map(|s| s.name).collect::<Vec<_>>(), ["build", "repro", "tests", "fmt"]);
    assert_eq!(steps[2].spec.display(), "npm test");
}

/// An operator repository with an installed (fake) Vitest and a run directory holding a
/// checkout with the same manifests.
fn install(dir: &Path, vitest: &str) -> (PathBuf, PathBuf) {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("node_modules/vitest")).unwrap();
    std::fs::write(repo.join("node_modules/vitest/package.json"), format!(r#"{{"version":"{vitest}"}}"#)).unwrap();
    std::fs::create_dir_all(repo.join("node_modules/dep/lib")).unwrap();
    std::fs::write(repo.join("node_modules/dep/lib/index.js"), "module.exports = 1\n").unwrap();
    let checkout = dir.join("run/checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    for base in [&repo, &checkout] {
        std::fs::write(base.join("package.json"), "{}").unwrap();
        std::fs::write(base.join("package-lock.json"), "{\"lockfileVersion\":3}").unwrap();
    }
    (repo, checkout)
}

#[test]
fn dependencies_are_copied_next_to_the_checkout_and_writes_stay_in_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, checkout) = install(dir.path(), "3.2.4");
    provision_node_modules(&repo, &checkout).unwrap();
    let run = checkout.parent().unwrap();
    assert!(JsVitest.check_environment(run).is_ok());
    assert!(!std::fs::symlink_metadata(run.join("node_modules")).unwrap().file_type().is_symlink());
    // A test or verification command writing through the run's node_modules changes only the
    // run's copy, never the operator's install.
    std::fs::write(run.join("node_modules/dep/lib/index.js"), "module.exports = 2\n").unwrap();
    std::fs::write(run.join("node_modules/dep/lib/new.js"), "").unwrap();
    assert_eq!(std::fs::read_to_string(repo.join("node_modules/dep/lib/index.js")).unwrap(), "module.exports = 1\n");
    assert!(!repo.join("node_modules/dep/lib/new.js").exists());
    // Provisioning again starts from a fresh copy.
    provision_node_modules(&repo, &checkout).unwrap();
    assert_eq!(std::fs::read_to_string(run.join("node_modules/dep/lib/index.js")).unwrap(), "module.exports = 1\n");
    assert!(!run.join("node_modules/dep/lib/new.js").exists());
}

#[cfg(unix)]
#[test]
fn links_inside_node_modules_point_into_the_copy() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, checkout) = install(dir.path(), "3.2.4");
    std::fs::create_dir_all(repo.join("node_modules/.bin")).unwrap();
    std::os::unix::fs::symlink("../dep/lib/index.js", repo.join("node_modules/.bin/dep")).unwrap();
    std::os::unix::fs::symlink(repo.join("node_modules/dep"), repo.join("node_modules/alias")).unwrap();
    provision_node_modules(&repo, &checkout).unwrap();
    let copy = checkout.parent().unwrap().join("node_modules");
    for link in ["alias", ".bin/dep"] {
        let target = dunce::canonicalize(copy.join(link)).unwrap();
        assert!(target.starts_with(dunce::canonicalize(&copy).unwrap()), "{link} -> {}", target.display());
    }
    std::fs::write(copy.join("alias/lib/index.js"), "changed").unwrap();
    assert_eq!(std::fs::read_to_string(repo.join("node_modules/dep/lib/index.js")).unwrap(), "module.exports = 1\n");
}

#[test]
fn provisioning_refuses_what_it_cannot_verify() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, checkout) = install(dir.path(), "3.2.4");
    // An install made for another lockfile (the operator's clone is on another branch).
    std::fs::write(repo.join("package-lock.json"), "{\"lockfileVersion\":3,\"x\":1}").unwrap();
    let err = provision_node_modules(&repo, &checkout).unwrap_err();
    assert!(err.contains("package-lock.json") && err.contains("reinstall"), "{err}");
    std::fs::write(repo.join("package-lock.json"), "{\"lockfileVersion\":3}").unwrap();
    std::fs::write(repo.join("yarn.lock"), "").unwrap();
    assert!(
        provision_node_modules(&repo, &checkout).unwrap_err().contains("yarn.lock"),
        "a lockfile only one side has"
    );
    std::fs::remove_file(repo.join("yarn.lock")).unwrap();
    provision_node_modules(&repo, &checkout).unwrap();
    // Workspaces, checked in the checkout under test as well as the operator's clone.
    std::fs::write(checkout.join("pnpm-workspace.yaml"), "packages: []\n").unwrap();
    assert!(provision_node_modules(&repo, &checkout).unwrap_err().contains("the checkout is a JavaScript workspace"));
    std::fs::remove_file(checkout.join("pnpm-workspace.yaml")).unwrap();
    // Unsupported Vitest majors.
    for version in ["2.1.9", "4.1.7"] {
        let (repo, checkout) = install(&dir.path().join(version), version);
        let err = provision_node_modules(&repo, &checkout).unwrap_err();
        assert!(err.contains(version) && err.contains("3.x"), "{err}");
    }
    // Not installed: an actionable error.
    std::fs::remove_dir_all(repo.join("node_modules")).unwrap();
    let err = provision_node_modules(&repo, &checkout).unwrap_err();
    assert!(err.contains("npm ci") && err.contains("no installed Vitest"), "{err}");
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

mod provisioned {
    use super::*;

    const IGNORE: &str = "needs node and LYRA_TEST_NODE_MODULES pointing at a node_modules with vitest 3";

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        run: PathBuf,
        checkout: PathBuf,
    }

    impl Fixture {
        /// An operator repository whose `node_modules` is a copy of the provisioned packages,
        /// and a LYRA run directory whose checkout holds the same files, provisioned by LYRA.
        fn new(files: &[(&str, &str)]) -> Self {
            let provisioned = PathBuf::from(std::env::var("LYRA_TEST_NODE_MODULES").expect(IGNORE));
            let provisioned = dunce::canonicalize(provisioned).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("repo");
            let modules = repo.join("node_modules");
            copy_tree((&provisioned, &modules), &provisioned, &modules, &mut Vec::new()).unwrap();
            let run = dir.path().join("run");
            let checkout = run.join("checkout");
            for (rel, text) in files {
                for base in [&repo, &checkout] {
                    let path = base.join(rel);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, text).unwrap();
                }
            }
            JsVitest.create_environment(&run).unwrap();
            let fx = Fixture { _dir: dir, repo, run, checkout };
            fx.provision();
            fx
        }
        fn provision(&self) {
            provision_node_modules(&self.repo, &self.checkout).unwrap();
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
        async fn discover(&self, spec: &CommandSpec, record: &ReproRecord) -> CommandOutput {
            let original = std::fs::read_to_string(self.checkout.join(&record.test_file)).unwrap();
            self.write(&record.test_file, "");
            let discovery = JsVitest.discovery(&JsVitest.reproduction_environment(spec.clone(), &self.run));
            let found = proc::run(&discovery.spec, Duration::from_secs(180), &CancellationToken::new()).await.unwrap();
            self.write(&record.test_file, &original);
            found
        }
        fn snapshot(&self, paths: &[&str]) -> HashMap<String, String> {
            paths.iter().map(|p| (p.to_string(), std::fs::read_to_string(self.checkout.join(p)).unwrap())).collect()
        }
    }

    const PACKAGE: &str = r#"{"name":"calc","type":"module","files":["src"],"devDependencies":{"vitest":"3"}}"#;
    const CALC: &str = "export function add(a, b) { return a - b }\n";
    const FIXED: &str = "export function add(a, b) { return a + b }\n";
    const TEST: &str = "import { describe, it, expect } from 'vitest'\nimport { add } from '../src/calc.js'\ndescribe('issue 7', () => {\n  it('sums two numbers', () => { expect(add(1, 2), 'sum must be three').toBe(3) })\n  it('other', () => { expect(1).toBe(1) })\n})\n";

    fn record_for(file: &str, name: &str) -> ReproRecord {
        let mut r = rec();
        r.test_file = file.into();
        r.test_name = name.into();
        r.command = regression_line(file, name, None);
        r
    }

    fn record() -> ReproRecord {
        record_for("tests/issue-7.test.js", NAME)
    }

    #[tokio::test]
    #[ignore = "needs node and LYRA_TEST_NODE_MODULES pointing at a node_modules with vitest 3"]
    async fn real_vitest_failure_binding_and_pass_after_fix() {
        let legacy = TEST.replace("'../src/calc.js'", "'../../src/calc.js'");
        let fx = Fixture::new(&[
            ("package.json", PACKAGE),
            ("src/calc.js", CALC),
            (
                "tests/existing.test.js",
                "import { it, expect } from 'vitest'\nit('existing', () => expect(1).toBe(1))\n",
            ),
            // Vitest's own file filter would also select this file, with the same test name.
            ("legacy/tests/issue-7.test.js", &legacy.replace("toBe(3)", "toBe(-1)")),
            ("tests/issue-7.test.js", TEST),
        ]);
        let r = record();
        let spec = JsVitest.regression_command(&r, &fx.checkout).unwrap();
        let run = fx.run(spec.clone()).await;
        let out = run.combined();
        assert_eq!(classify_report(&out, &r), RegressionOutcome::Failed, "{out}");
        confirm_failure_in(&run, &r).unwrap();
        assert_eq!(JsVitest.loaded_test_inputs(&out, &r, &fx.checkout).unwrap(), Vec::<String>::new(), "{out}");
        assert!(
            !fx.repo.join("node_modules/.vite").exists() && !fx.run.join("node_modules/.vite").exists(),
            "no cache in node_modules"
        );

        // Source binding: with the test file emptied, it is collected and holds no test.
        let found = fx.discover(&spec, &r).await;
        assert!(found.success(), "{}", found.combined());
        assert_absent(&found.stdout, &r).map_err(|e| format!("{e}\n{}", found.combined())).unwrap();

        // The fix passes the regression and the suite.
        fx.write("src/calc.js", FIXED);
        let out = fx.run(spec).await.combined();
        assert_passed_in(&out, &r).unwrap();
        let steps = JsVitest.verification_steps(&VerifyCommands::default(), &r, &fx.checkout).unwrap();
        fx.write("legacy/tests/issue-7.test.js", &legacy);
        let suite = fx.run(steps[1].spec.clone()).await;
        assert!(suite.success(), "{}", suite.combined());
        let report = parse_report(&suite.combined()).unwrap();
        assert!(report.tests.is_empty() && report.loaded.is_empty(), "the suite reports counts only");
    }

    #[tokio::test]
    #[ignore = "needs node and LYRA_TEST_NODE_MODULES pointing at a node_modules with vitest 3"]
    async fn selectors_stay_literal_and_the_assertion_must_carry_the_expected_text() {
        // A dash-led file name and a title containing the ` > ` separator.
        let edge = "import { describe, it, expect } from 'vitest'\nimport { add } from './src/calc.js'\ndescribe('issue 7', () => {\n  it('rejects x > y', () => { expect(add(1, 2), 'sum must be three').toBe(3) })\n  it('rejects x', () => { expect(1).toBe(1) })\n})\n";
        let fx = Fixture::new(&[("package.json", PACKAGE), ("src/calc.js", CALC), ("--edge.test.js", edge)]);
        let r = record_for("--edge.test.js", "issue 7 > rejects x > y");
        let spec = JsVitest.regression_command(&r, &fx.checkout).unwrap();
        let run = fx.run(spec).await;
        assert_eq!(classify_report(&run.combined(), &r), RegressionOutcome::Failed, "{}", run.combined());
        confirm_failure_in(&run, &r).unwrap();

        // The expected text printed by the test while it fails for another reason is not evidence.
        let logs = "import { describe, it } from 'vitest'\ndescribe('issue 7', () => {\n  it('rejects x > y', () => { console.log('sum must be three'); throw new TypeError('unrelated') })\n})\n";
        fx.write("--edge.test.js", logs);
        let run = fx.run(JsVitest.regression_command(&r, &fx.checkout).unwrap()).await;
        assert!(guards::output_contains_expected(&run.combined(), &r.expected), "precondition: {}", run.combined());
        let err = confirm_failure_in(&run, &r).unwrap_err();
        assert!(err.contains("own error"), "{err}");

        // A broken suite teardown next to the selected failure is not a clean reproduction.
        let teardown = "import { describe, it, expect, afterAll } from 'vitest'\nimport { add } from './src/calc.js'\ndescribe('issue 7', () => {\n  describe('inner', () => {\n    afterAll(() => { throw new Error('teardown broke') })\n    it('rejects x > y', () => { expect(add(1, 2), 'sum must be three').toBe(3) })\n  })\n})\n";
        fx.write("--edge.test.js", teardown);
        let r = record_for("--edge.test.js", "issue 7 > inner > rejects x > y");
        let run = fx.run(JsVitest.regression_command(&r, &fx.checkout).unwrap()).await;
        assert_eq!(classify_report(&run.combined(), &r), RegressionOutcome::SetupFailed, "{}", run.combined());
        assert!(confirm_failure_in(&run, &r).is_err());

        // A test printing a forged, passing result makes the evidence inconclusive.
        let forged = format!(
            "import {{ it }} from 'vitest'\nit('rejects x > y', () => {{ process.stdout.write({:?}) }})\n",
            report(json!([test(NAME, "run", Some("pass"))]), json!([]), 0)
        );
        fx.write("--edge.test.js", &forged);
        let r = record_for("--edge.test.js", "rejects x > y");
        let out = fx.run(JsVitest.regression_command(&r, &fx.checkout).unwrap()).await.combined();
        assert!(parse_report(&out).is_none(), "{out}");
        assert!(assert_passed_in(&out, &r).is_err());
    }

    #[tokio::test]
    #[ignore = "needs node and LYRA_TEST_NODE_MODULES pointing at a node_modules with vitest 3"]
    async fn a_helper_the_regression_loads_is_frozen_and_editing_it_is_rejected() {
        let test = "import { describe, it, expect } from 'vitest'\nimport { add } from '../src/calc.js'\nimport { EXPECTED } from './helpers.js'\ndescribe('issue 7', () => {\n  it('sums two numbers', () => { expect(add(1, 2), 'sum must be three').toBe(EXPECTED) })\n})\n";
        let fx = Fixture::new(&[
            ("package.json", PACKAGE),
            ("src/calc.js", CALC),
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
    #[ignore = "needs node and LYRA_TEST_NODE_MODULES pointing at a node_modules with vitest 3"]
    async fn configured_setup_files_are_frozen_by_path_and_a_nondefault_config_is_used() {
        // The setup file has no conventional name and lives in declared production code; the
        // configuration is not at a default location.
        let config = "export default { test: { setupFiles: ['./src/init.js'] } }\n";
        let init = "globalThis.EXPECTED = 3\n";
        let test = "import { describe, it, expect } from 'vitest'\nimport { add } from '../src/calc.js'\ndescribe('issue 7', () => {\n  it('sums two numbers', () => { expect(add(1, 2), 'sum must be three').toBe(globalThis.EXPECTED) })\n})\n";
        let fx = Fixture::new(&[
            ("package.json", PACKAGE),
            ("lyra.toml", "[profiles.js-vitest]\nconfig = \"config/unit.mjs\"\n"),
            ("config/unit.mjs", config),
            ("src/calc.js", CALC),
            ("src/init.js", init),
            ("tests/issue-7.test.js", test),
        ]);
        let r = record();
        let spec = JsVitest.regression_command(&r, &fx.checkout).unwrap();
        assert!(spec.display().ends_with("--config config/unit.mjs"), "{}", spec.display());
        let run = fx.run(spec.clone()).await;
        let out = run.combined();
        assert_eq!(classify_report(&out, &r), RegressionOutcome::Failed, "the configured setup ran: {out}");
        confirm_failure_in(&run, &r).unwrap();
        let loaded = JsVitest.loaded_test_inputs(&out, &r, &fx.checkout).unwrap();
        assert_eq!(loaded, ["src/init.js"], "{out}");
        let frozen = fx.snapshot(&["package.json", "lyra.toml", "src/calc.js", "src/init.js", "tests/issue-7.test.js"]);
        fx.write("src/init.js", "globalThis.EXPECTED = -1\n");
        let read_checkpoint = |p: &str| frozen.get(p).cloned();
        let read_now = |p: &str| std::fs::read_to_string(fx.checkout.join(p)).ok();
        let record = ReproRecord { frozen_inputs: loaded, command: spec.display(), ..r.clone() };
        let change = vec![("M".to_string(), "src/init.js".to_string())];
        let err = check_fix_changes_with(&change, &record, &read_checkpoint, &read_now).unwrap_err();
        assert!(err.contains("src/init.js"), "{err}");
        fx.write("src/init.js", init);

        // A setup failure during discovery is a failed discovery, not an empty file.
        fx.write("src/init.js", "throw new Error('setup exploded')\n");
        let found = fx.discover(&spec, &r).await;
        let err = assert_absent(&found.stdout, &r).unwrap_err();
        assert!(err.contains("failed instead of finding no test"), "{err}");
    }

    #[tokio::test]
    #[ignore = "needs node and LYRA_TEST_NODE_MODULES pointing at a node_modules with vitest 3"]
    async fn a_large_suite_reports_a_bounded_result() {
        let many = "import { it, expect } from 'vitest'\nit.each(Array.from({ length: 20000 }, (_, i) => i))('case %i', (i) => { expect(i).toBe(i) })\n";
        let fx = Fixture::new(&[("package.json", PACKAGE), ("src/calc.js", FIXED), ("tests/many.test.js", many)]);
        let steps = JsVitest.verification_steps(&VerifyCommands::default(), &record(), &fx.checkout).unwrap();
        let suite = fx.run(steps[1].spec.clone()).await;
        assert!(suite.success() && !suite.output_truncated, "{}", suite.combined());
        let line = suite.stdout.lines().find(|l| l.starts_with(RESULT_PREFIX)).unwrap();
        assert!(line.len() < 4096, "{} bytes", line.len());
        assert!(line.contains("\"passed\":20000"), "{line}");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "needs node and LYRA_TEST_NODE_MODULES pointing at a node_modules with vitest 3"]
    async fn code_resolved_from_outside_the_checkout_fails_closed_in_repro_and_verify() {
        // A package link in the operator's node_modules points back into the operator's clone
        // (as a workspace link would): there the code is fixed, in the checkout it is buggy.
        let fx = Fixture::new(&[
            ("package.json", PACKAGE),
            ("src/calc.js", CALC),
            ("tests/issue-7.test.js", &TEST.replace("'../src/calc.js'", "'calc-self/calc.js'")),
        ]);
        std::fs::write(fx.repo.join("src/calc.js"), FIXED).unwrap();
        std::fs::write(fx.repo.join("src/package.json"), r#"{"name":"calc-self","type":"module"}"#).unwrap();
        std::os::unix::fs::symlink(fx.repo.join("src"), fx.repo.join("node_modules/calc-self")).unwrap();
        fx.provision();
        let r = record();
        let out = fx.run(JsVitest.regression_command(&r, &fx.checkout).unwrap()).await.combined();
        assert_eq!(classify_report(&out, &r), RegressionOutcome::Passed, "precondition: the clone's code ran: {out}");
        let err = JsVitest.loaded_test_inputs(&out, &r, &fx.checkout).unwrap_err();
        assert!(err.contains("outside the checkout") && err.contains("src/calc.js"), "{err}");
        let err = assert_passed_in(&out, &r).unwrap_err();
        assert!(err.contains("outside the checkout") && err.contains("src/calc.js"), "verify too: {err}");
    }
}
