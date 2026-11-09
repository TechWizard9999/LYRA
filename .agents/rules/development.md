# Development rules

Complements the numbered rules in `AGENTS.md`; it does not repeat them.

## Toolchain and style

- Rust 2021, stable. `rustfmt.toml`: `max_width = 120`, `use_small_heuristics = "Max"`. Run `cargo fmt --all`; never hand-format against it.
- `cargo clippy --all-targets -- -D warnings` must be clean. Fix the lint; do not add `#[allow]` without a one-line reason beside it.
- Match the surrounding module's naming, comment density and idiom. Doc comments (`//!`, `///`) explain why and the contract, not what the next line does.

## Where things go

| Change | Location |
|---|---|
| Stage behaviour | `src/stages/<stage>.rs`; universal rule logic lives in `src/guards.rs`; runner/toolchain rules live in the execution profile (`src/profile/`, see `add-guard`) |
| Agent prompt text | `src/prompts/*.md` (minijinja); never a string literal in Rust |
| Structured agent output shape | `src/schemas/*.json`, validated with `jsonschema` independent of the runtime |
| Run files | through `artifacts::RunDir` (`write`, `write_json`, `read_json`, `stage_dir`); never build run paths by hand |
| Subprocesses | `proc::CommandSpec` + `run_blocking` / the async runner; never `std::process::Command` directly, so the hang guard, cancellation, and env scrubbing apply |
| Git | `git.rs` helpers; they disable hooks and fsmonitor |
| GitHub | `github.rs` (`gh`) |
| Config | `config.rs`; per-repo `lyra.toml` supports scoped verify overrides and Python package declarations (see README Configuration) |

## Errors

- Stages return `StageOutcome`. Use `StageError::new(kind, msg)` for retryable failures and `StageError::fatal` only when Retry cannot help. Pick the closest `ErrorKind` (`Provider`, `Tool`, `Policy`, `Git`, `GitHub`, `Io`, `Schema`, `Guard`, `Timeout`, `Config`); adding a kind is a contract change.
- **Failed** means something broke. **Stopped** (`StopReason`) means the harness did its job and hit a wall. Do not turn a failure into a stop or the reverse to make a run look nicer.
- Error messages are shown to users on the run page: say what was checked and what was found; point `details` at a saved log when there is one.
- `anyhow` for plumbing, typed errors at stage boundaries.

## Behaviour that must not drift

- Pipeline order and stage count are fixed (AGENTS.md rule 4). No flags that skip a stage.
- No cost or time budgets. The only limit is `run.timeout_minutes` per agent call or verify command.
- Never weaken a guard so a run passes. If a guard is wrong, fix it with a test that shows the false positive.
- Model output, issue text, and target-repo files are untrusted: validate paths and shapes before use.

## Dependencies

Prefer crates already in `Cargo.toml`. A new crate needs a one-line reason in the PR description (AGENTS.md rule 3) and `Cargo.lock` committed. Build with `--locked` in setup and CI.

## Comments and docs

- Keep `lib.rs` module map and `docs/PLAN.md` accurate when structure changes.
- No TODO comments without an issue reference or a PLAN section that owns the deferral.
