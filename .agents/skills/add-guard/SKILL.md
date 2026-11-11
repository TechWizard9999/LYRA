---
name: add-guard
description: Add or tighten a guard rule (repro diff rules, expected-assertion match, test lock, fix scope, verify output parsing), deciding whether it is universal (src/guards.rs) or runner-specific (an execution profile in src/profile/), with test-first discipline. Use when a run should have been rejected or was wrongly rejected.
---

# Add or change a guard

Guards are the harness's trust. They exist to stop an agent from "passing" without honestly reproducing and fixing. Never loosen one to make a run pass.

## Read first
- `docs/PLAN.md` §3.1 (repro), §3.3 (fix), §3.4 (verify) and §13 (execution profiles).
- `.agents/rules/security.md` (untrusted agent output).
- `src/guards.rs`: the **universal** rules, true for any language or test runner.
- `src/profile/mod.rs`: the runner-neutral `ExecutionProfile` contract; the built-in profiles `src/profile/rust_cargo.rs` (every Cargo/libtest rule), `src/profile/python_pytest.rs` and `src/profile/js_vitest.rs`.

## Decide where the rule belongs
Ask: would this rule still be correct for a repository that is not Cargo, with a different test runner?

| Universal: `src/guards.rs` | Runner-specific: the profile (`src/profile/<id>.rs`) |
|---|---|
| repository-relative path validation (`validate_repro_path`, `norm_path`) | test identity and selector syntax (`validate_record`, `regression_command`) |
| symlink and worktree-escape checks on repro paths | reading test output: `classify`, `confirm_failure`, `assert_passed`, `assert_absent_from_discovery` (in `rust-cargo`: `classify_cargo_test`, `assert_test_passed`) |
| expected-text match (`output_contains_expected`) | which paths are tests, registration conventions (`is_test_path`, module registration and the `syn` AST audit) |
| byte lock of the frozen test file (`frozen_test_violations`) | manifest and dependency rules (`Cargo.toml` dev-dependencies, `Cargo.lock` additions only, `[[test]]`, `autotests`, `.cargo/` config) via `check_repro_changes` and `check_fix_changes` |
| change-scope accounting (`scope_from_diff`, `LARGE_CHANGE_LINES`) | verify commands, environment preparation, failure explanations |

`guards::check_repro_changes` and `guards::check_fix_changes` are the entry points the stages call: they apply the universal checks, then delegate the runner-specific diff rules to the profile of the same name. Nothing in `guards.rs`, the engine, the stages or the dashboard may name Cargo; if a rule needs `cargo`, `Cargo.toml`, libtest wording or a registration line, it goes in the profile. A new language is a new profile module, not new branches in `guards.rs`.

## Steps
1. **Write the failing test first**, in the module that will own the rule: `guards.rs`'s `tests` for a universal rule, the profile's `tests` for a runner rule. Use a realistic input: real runner output, a real diff, a real manifest snippet. If a guard missed a bad run, reproduce that exact input.
2. State the rule in one sentence, then implement it as a pure function returning `Result<(), String>` or a typed result. No I/O, no clock, no env; callers pass in file contents (the `read_old` / `read_new` closures).
3. Normalize paths with `norm_path` before comparing. Cover Windows separators and CRLF.
4. Fail closed: unknown shapes, truncated output and parse errors are rejections, not passes. Truncated output must never count as successful verification.
5. Make the message actionable: the user sees it on the run page. Say what was found and which rule it broke.
6. Wire it in: a universal rule inside the `guards.rs` entry points; a runner rule inside the profile method of the same purpose. Add a stage-level test that the stage returns the right `Failed`/`Stopped` outcome.
7. If you touched the `ExecutionProfile` trait itself, that is a contract change (`change-contract`): bump `CONTRACT_VERSION` only when saved runs would be misread, and keep `for_state` mapping old runs explicitly.
8. Cases to include at minimum: passes, each rejection reason, boundary (empty, one-line, huge), and a regression for the incident that prompted the change. For a runner rule, also one test showing the universal layer still rejects the same input when the profile would accept it.

## What the `rust-cargo` profile locks in S4 (don't regress)
The repro test file and its registration line, `#[ignore]`/`cfg` gates on it, `[dev-dependencies]`, `[[test]]`, `autotests`, `.cargo/config*`, and the recorded test command. Other profiles define their own lock set; the universal byte lock on the frozen test file applies to all.

## Verify
`pre-pr-check`. If a rule is added, removed or relaxed, update `docs/PLAN.md` §3.x (and §13 if the boundary moved) in the same PR and call it out in the PR description.
