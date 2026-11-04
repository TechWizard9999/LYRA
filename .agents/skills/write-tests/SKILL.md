---
name: write-tests
description: Write unit tests for LYRA code that follow repo conventions (offline, tempfile, LYRA_HOME, fake agent binaries, recorded fixtures). Use whenever adding or changing behaviour, or fixing a bug.
---

# Write tests

Rules are in `.agents/rules/testing.md`; this is the working procedure.

## Steps
1. **Locate the module's `#[cfg(test)] mod tests`** (create it if the module has none; every module needs one).
2. **Bug fix:** write the test first, run it, confirm it fails for the right reason, then fix. Mention this in the PR.
3. **Pick the harness:**
   - Pure logic (guards, parsers, config): plain functions with literal inputs; copy real output into the test.
   - Run directory / artifacts / state: `let dir = tempfile::tempdir().unwrap();` then `RunDir::new(dir.path())`. Never `~/.lyra`; set `LYRA_HOME` only in tests that go through `config::lyra_home`, and restore it or use an explicit home path instead where possible.
   - Git behaviour: a scratch repo in a tempdir built with the `git.rs` helpers; set `user.name`/`user.email` locally so the test doesn't depend on the machine's identity.
   - Agent calls: a fake binary script in a tempdir that prints a recorded fixture (`fake_binary` in `providers/claude.rs`, `providers/codex.rs`); or a test `Agent` impl returning canned `AgentOutput`.
   - HTTP: build the router, call it with `tower::ServiceExt::oneshot`, read bodies with `http-body-util`.
   - Browser JS: add to `src/web/static/app.test.js`.
4. **Cover:** the happy path, every error/stop branch, edge cases (empty, truncated, CRLF, Windows separators), and cancellation/timeouts where relevant (use `CancellationToken` and short timeouts, not sleeps).
5. **Assert specifics:** `ErrorKind`, `retryable`, `StopReason`, exact messages that users see, serialized tags (`"outcome": "stopped"`).
6. **Cross-platform:** helper executables need both `#[cfg(windows)]` (`.cmd`) and `#[cfg(not(windows))]` (`.sh` + `chmod 755`) variants.
7. **Clean:** no network, no `gh`, no real agent CLI, no leftover files outside tempdirs.

## Run
```sh
cargo test <module_or_test_name>      # fast loop
cargo test                            # whole suite
node --test src/web/static/app.test.js
```
Then `pre-pr-check`.

## Smells to fix
A test that passes with the fix reverted; assertions only on `is_ok()`; tests that depend on order or the machine's git config; fixtures with secrets or absolute home paths.
