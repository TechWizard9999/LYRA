---
name: add-guard
description: Add or tighten a guard rule in src/guards.rs (repro diff rules, expected-assertion match, test lock, fix scope, verify output parsing) with test-first discipline. Use when a run should have been rejected or was wrongly rejected.
---

# Add or change a guard

Guards are the harness's trust. They exist to stop an agent from "passing" without honestly reproducing and fixing. Never loosen one to make a run pass.

## Read first
- `docs/PLAN.md` §3.1 (repro), §3.3 (fix), §3.4 (verify).
- `src/guards.rs`: `check_repro_changes[_with]`, `check_fix_changes[_with]`, `classify_cargo_test`, `assert_test_passed`, `output_contains_expected`, registration-line and Cargo.toml/Cargo.lock helpers, `norm_path`, `is_test_path`.
- `.agents/rules/security.md` (untrusted agent output).

## Steps
1. **Write the failing test first** in `guards.rs`'s `tests` module, using a realistic input: real `cargo test` output, a real diff, a real `Cargo.toml` snippet. If the guard missed a bad run, reproduce that exact input.
2. State the rule in one sentence, then implement it as a pure function returning `Result<(), String>` or a typed result. No I/O, no clock, no env; callers pass in file contents.
3. Normalize paths with `norm_path` before comparing. Cover Windows separators and CRLF.
4. Fail closed: unknown shapes, truncated output, and parse errors are rejections, not passes. Truncated output must never count as successful verification.
5. Make the message actionable: the user sees it on the run page. Say what was found and which rule it broke.
6. Wire it into the stage (`repro`, `fix`, or `verify`) and add a stage-level test that the stage returns the right `Failed`/`Stopped` outcome.
7. Cases to include at minimum: passes, each rejection reason, boundary (empty, one-line, huge), and a regression for the incident that prompted the change.

## Locked things in S4 (don't regress)
The repro test file and its registration line, `#[ignore]`/`cfg` gates on it, `[dev-dependencies]`, `[[test]]`, `autotests`, `.cargo/config*`, and the recorded test command.

## Verify
`pre-pr-check`. If a rule is added, removed or relaxed, update `docs/PLAN.md` §3.x in the same PR and call it out in the PR description.
