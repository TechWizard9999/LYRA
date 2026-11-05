# AGENTS.md: working on LYRA

This is the canonical guide for anyone (human or coding agent) changing this repository. `CLAUDE.md` and `.github/copilot-instructions.md` point here.

## Purpose

LYRA is a Rust harness around a coding-agent CLI (Claude Code or Codex). Given a path to an already-cloned repository (Rust via the built-in `rust-cargo` profile today; other languages through execution profiles, PLAN §13) and a GitHub issue, it runs one fixed pipeline (intake → context → repro → rca → fix → verify → draft PR), enforces guards between the stages, saves every step under `~/.lyra/runs/<id>/`, and streams progress to a local dashboard. The design is [docs/PLAN.md](docs/PLAN.md); deferred designs are in [docs/RESEARCH.md](docs/RESEARCH.md).

## Architecture map

| Module | Role |
|---|---|
| `src/main.rs` | CLI: `serve`, `run`, `retry`, `runs`, `doctor`, `config` |
| `src/config.rs` | `~/.lyra/config.toml` (`LYRA_HOME` override) + `<repo>/lyra.toml` (verify overrides only) |
| `src/events.rs` | `StageId`, `Event`/`EventKind`/`AgentEvent`, the append-only `events.jsonl` writer and live broadcast |
| `src/state.rs` | `run.json`: `RunRequest`, `RunState`, statuses, checkpoints, attempts |
| `src/error.rs` | `StageError`, `ErrorKind`, `StopReason`, `StageOutcome` |
| `src/engine.rs` | fixed pipeline driver, one active run, retry-from-stage, cancel, interrupted marking |
| `src/stages/` | S0..S6: `intake`, `context`, `repro`, `rca`, `fix`, `verify`, `pr` (`mod.rs` = `Stage` trait + `StageCtx`) |
| `src/guards.rs` | universal repro/fix rules, expected-assertion match, scope accounting; runner rules live in the profile |
| `src/profile/` | the `ExecutionProfile` contract, resolution and legacy mapping (`mod.rs`); the built-in `rust-cargo` profile (`rust_cargo.rs`): Cargo/libtest commands, output parsing, Cargo-specific guards and verification environment |
| `src/providers/` | the `Agent` trait (`mod.rs`) and the Claude Code / Codex adapters |
| `src/proc.rs` | subprocesses: hang-guard timeout, cancellation, process-tree kill, Windows `.cmd` shims |
| `src/git.rs`, `src/github.rs` | git worktrees/checkpoints; `gh` calls |
| `src/artifacts.rs` | run directory layout and artifact I/O |
| `src/doctor.rs` | environment checks (`lyra doctor`, guarded `POST /api/doctor`) |
| `src/web/` | dashboard: `server.rs` (router, pages), `api.rs` (JSON API), `sse.rs` (event stream), `views.rs` (maud), `static/` (CSS, JS) |
| `src/prompts/*.md`, `src/schemas/*.json` | prompt templates (minijinja) and structured-output schemas |

Run directory (PLAN §7):

```
~/.lyra/runs/<run-id>/
  run.json  events.jsonl
  worktree/      branch lyra/issue-<n>-<run-id>
  artifacts/     00-issue.md  10-context/  20-repro/  30-rca/  40-fix/  50-verify/  60-pr/  transcripts/
  attempts/      superseded stage outputs after a retry
```

## Rules

1. **Rust 2021, stable toolchain.** `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` must be clean.
2. **Unit tests in every module** (`#[cfg(test)] mod tests`). New behaviour comes with a test; bug fixes come with a test that failed before the fix.
3. **No new dependencies** without a one-line reason in the PR description. Prefer the crates already in `Cargo.toml`.
4. **Keep the fixed pipeline**: one flow end to end, intake → context → repro → rca → fix → verify → draft PR. No skipping stages, no alternative flows, no plugin system.
5. **No cost or time budgets.** The only limit is the hang-guard `run.timeout_minutes` per agent call / verify command, plus Cancel.
6. **LYRA never clones repositories.** It takes a path to an already-cloned repo and works in its own git worktree.
7. **Never auto-merge, never non-draft PRs.** The PR is created as a draft for the exact verified commit.
8. **No AI co-author trailers** in commits LYRA makes or in commits to this repo. Commits use the operator's git identity.
9. **On Windows, run cargo from PowerShell** (or cmd). Git Bash puts its own `link` ahead of the real linker on `PATH`.
10. **Contracts change only with a note in the PR**: `error`, `events`, `state`, `config`, `artifacts`, `proc`, `git`, `providers/mod.rs`, `stages/mod.rs`, `engine`. They are shared by every stage, the adapters, the CLI and the dashboard; `run.json` and `events.jsonl` on disk must stay readable.
11. **Prompts live in `src/prompts/`, schemas in `src/schemas/`.** Do not inline prompt text or schemas in Rust code.
12. **Artifacts follow the PLAN §7 layout** (`artifacts/<NN-stage>/...`). A new artifact goes under its stage's directory and is announced with an `artifact_written` event.
13. **Read-only stages stay read-only** (file reading and search only; the harness supplies command output). Guards are not weakened to make a run pass.
14. **Follow the target repository's own rules** (CONTRIBUTING, AI policy, AGENTS.md) as written; no blanket skip rules.
15. **Target-language and test-runner logic lives in `src/profile/`**; stages, guards, engine and dashboard stay runner-neutral and call the `ExecutionProfile` contract. A new profile is a built-in module plus tests, not a runtime plugin.

## Running tests

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests must not need network access, `gh` authentication or a runtime installed; use `tempfile` directories and recorded fixtures (`tests/fixtures/`). Use `LYRA_HOME` or an explicit home path, never the real `~/.lyra`.

## What "done" means for a change

- It builds, `cargo fmt` / `clippy -D warnings` / `cargo test` are clean.
- New or changed behaviour has unit tests in the module it touches.
- The pipeline order, guards and draft-only PR behaviour are unchanged unless the change is explicitly about them and PLAN.md is updated in the same PR.
- Contract changes (rule 10) and new dependencies (rule 3) are called out in the PR description.
- User-visible changes are reflected in README.md / SETUP.md.
- Commits carry no AI co-author trailers.
