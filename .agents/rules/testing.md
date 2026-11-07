# Testing rules

## What every test must satisfy

- **Offline.** No network, no `gh` authentication, no installed Claude or Codex CLI. CI and a fresh laptop must pass the same way.
- **Isolated.** Use `tempfile::tempdir()` for run dirs and scratch repos. Set `LYRA_HOME` or pass an explicit home path; never touch the real `~/.lyra`.
- **Local to the module.** Unit tests live in a `#[cfg(test)] mod tests` in the file they cover. Every module has one.
- **Fails first for bug fixes.** A fix PR contains a test that fails on the old code. Say so in the PR.
- **Cross-platform.** Anything spawning a helper must work on Windows and Unix: branch with `#[cfg(windows)]` / `#[cfg(not(windows))]` like the `fake_binary` helpers in `providers/claude.rs` and `providers/codex.rs`.

## Patterns already in the repo

| Need | Use |
|---|---|
| Fake agent CLI | a tiny script (`.sh` / `.cmd`) in a tempdir that `cat`s a recorded JSONL fixture, as in `fake_binary` |
| Recorded runtime output | `tests/fixtures/claude-stream.jsonl`, `tests/fixtures/codex-exec.jsonl` (see `record-fixture` skill) |
| Scratch git repo | `tempfile` + `git init` via the `git.rs` helpers, as in `stages/repro.rs` fixtures |
| Event/state round trips | serialize to JSON and back; assert tags such as `outcome`, `reason` (see `error.rs`) |
| Web handlers and SSE | `tower` + `http-body-util` (dev-dependencies) against the router; no real socket needed |
| Dashboard JS | `node --test src/web/static/app.test.js` (Node's built-in runner, no packages) |

## What to assert

- Guards: one passing case, one case per rejection reason, and the edge cases (CRLF, Windows separators via `norm_path`, empty input, truncated output).
- Parsers (`parse_summaries`, stream parsers): real captured output, plus a truncated and a garbled variant.
- State and events: old on-disk files still load (new fields are `#[serde(default)]`).
- Error paths: assert the `ErrorKind`, `retryable`, and the `StopReason`, not just "is err".

## Don'ts

- No `sleep`-based timing; use cancellation tokens and short timeouts.
- No tests that need `cargo` to download crates from the network inside a temp project. Use `--offline` fixtures or avoid compiling.
- Don't weaken or delete a failing test to get green; fix the cause or ask.
- Don't commit large captured transcripts; trim fixtures to the events the test needs and strip secrets and absolute home paths.

## The gate

`pre-pr-check` skill, or:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
node --test src/web/static/app.test.js
```

End-to-end behaviour against a real runtime is the `acceptance-run` skill; it is manual and not part of CI.
