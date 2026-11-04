---
name: review-checklist
description: Review a LYRA diff or PR against the project's invariants (fixed pipeline, guards, read-only stages, contracts, isolation, tests, docs). Use when reviewing your own change or someone else's.
---

# Review checklist

Get the diff (`git diff main...HEAD` or the PR). Read it fully before judging. Report findings as `path:line: severity: problem. fix.`; skip style nits that `cargo fmt`/clippy already cover. Blocking items first.

## Invariants (any "no" is blocking)
- [ ] Pipeline is still intake → context → repro → rca → fix → verify → draft PR; no skip path, alternate flow, or plugin hook.
- [ ] No cost or time budgets added; only `run.timeout_minutes` and Cancel.
- [ ] LYRA still never clones; PRs are draft only; PR head == `verified_commit`; nothing auto-merges.
- [ ] No guard weakened, bypassed, or made advisory. Fail-closed on unknown/truncated input.
- [ ] S1/S3 remain read-only with the change-detection scan intact.
- [ ] Locked items in S4 still locked (test file, registration, dev-deps, `[[test]]`, `autotests`, `.cargo/config*`, test command).
- [ ] Prompts only in `src/prompts`, schemas only in `src/schemas`; untrusted input stays inside UNTRUSTED DATA blocks.
- [ ] Artifacts under `artifacts/<NN-stage>/`, written via `RunDir`, `artifact_written` emitted.

## Contracts and compatibility
- [ ] Touches `error`, `events`, `state`, `config`, `artifacts`, `proc`, `git`, `providers/mod.rs`, `stages/mod.rs`, or `engine`? Then the PR has a Contract change note.
- [ ] Old `run.json`, `events.jsonl`, and `config.toml` still load (new fields `#[serde(default)]`, no removed/renamed variants). Test with an old-shape literal.
- [ ] Dashboard handles new variants.

## Security
- [ ] Model-supplied paths validated (no abs, `..`, `:`, `.git`, canonical inside worktree).
- [ ] Subprocesses go through `proc` (hang guard, cancel, env scrubbing). Provider isolation flags intact; `extra_args` allowlist not widened.
- [ ] New web routes pass Host/Origin checks; GET has no side effects; blocking work off async threads; no `innerHTML` with run data.
- [ ] No secrets or home paths in logs, artifacts, fixtures, PR bodies.

## Correctness and tests
- [ ] Failed vs stopped used correctly; `ErrorKind` and `retryable` sensible; messages actionable.
- [ ] Cancellation and timeout paths handled; temp dirs cleaned up (also on cancel).
- [ ] Retry-from-stage still valid after the change.
- [ ] Windows: path normalization, CRLF, `cfg(windows)` helpers where processes are spawned.
- [ ] Tests exist, are offline, use tempdirs, and fail without the change. Edge and error cases covered, not only the happy path.

## Process
- [ ] New dependency has a reason; `Cargo.lock` updated.
- [ ] README/SETUP/PLAN updated where behaviour or design changed.
- [ ] Commits: no AI co-author trailers; sensible messages; `.claude/skills` synced if `.agents/skills` changed (`agents.sh check`).

## Output
Summarize: verdict (approve / changes requested), blocking issues, non-blocking suggestions. Be specific and cite lines; don't pad with praise.
