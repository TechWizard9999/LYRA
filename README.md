# LYRA

LYRA is a local harness around an off-the-shelf coding agent (Claude Code or Codex CLI). You point it at a Rust repository you have already cloned and a GitHub issue; it gathers context, has the agent write a failing regression test in its own file, finds the root cause in a read-only pass, has the agent fix the production code while the test is locked, verifies the fix by actually running the test and the workspace checks, and opens a **draft** PR for the exact commit that passed. Every step is checkpointed, written to disk and streamed live to a dashboard on `localhost`. When it cannot finish (the bug does not reproduce, the root cause stays unresolved, verification fails) it stops with the evidence and a Retry button.

## Flow

```
S0 intake    gh issue view (with comments) → 00-issue.md ; worktree ~/.lyra/runs/<id>/worktree
             branch lyra/issue-<n>-<run-id> from the default branch ; base commit recorded
             CONTRIBUTING / AI-policy excerpts shown on the run page
S1 context   harness collects related issues/PRs, git log -S, blame, cargo metadata, AGENTS.md/CLAUDE.md
             one read-only agent call (Read/Grep/Glob only) → 10-context/summary.json
S2 repro     agent writes a failing test in its own file ; harness runs it ; must fail on the recorded assertion ; formatted, committed
S3 rca       agent in read-only mode, harness supplies history + repro logs → 30-rca/rca.json (supported | unresolved)
S4 fix       agent edits production code ; test file and its registration locked ; commit
S5 verify    cargo build ; the repro test is executed and passes ; cargo test ; cargo fmt --check ; verified SHA recorded
S6 draft PR  body from template + artifacts ; push verified SHA ; find-or-create PR for that exact head
```

## Quick start

```sh
git clone <this repo> lyra && cd lyra
./setup.ps1          # Windows (PowerShell)
./setup.sh           # Linux / macOS
```

The setup script checks your tools, builds `target/release/lyra`, writes `~/.lyra/config.toml` if it does not exist, and runs `lyra doctor`. Full prerequisites and troubleshooting are in [SETUP.md](SETUP.md).

```sh
lyra doctor          # runtime on PATH + auth, gh auth status, cargo
lyra serve           # dashboard on http://127.0.0.1:7700 (opens the browser)
```

In the browser: enter the path of a cloned Rust repository, the issue (`https://github.com/owner/repo/issues/123`, `#123` or `123`), pick the runtime, tick the policy box if the repository has a contribution / AI policy you have read, and press **Run**. The run page shows the stage strip, the agent's live log (tool calls, text, file edits), the artifacts, and any error.

From a terminal instead:

```sh
lyra run --repo /path/to/cloned/repo --issue https://github.com/owner/repo/issues/123
lyra run --repo . --issue 42 --issue-file issue.md     # offline: issue text from a file
lyra runs                                              # list runs
lyra retry <run-id> --from fix                         # intake|context|repro|rca|fix|verify|pr
lyra config                                            # print the effective configuration
```

## Configuration

`~/.lyra/config.toml` (or `$LYRA_HOME/config.toml`). Created with these defaults on first use:

```toml
[run]
provider = "claude"            # "claude" | "codex"
timeout_minutes = 30           # hang guard per agent call / verify command

[server]
bind = "127.0.0.1:7700"
open_browser = true

[providers.claude]
kind = "claude"
auth = "api_key"               # "api_key" (needs ANTHROPIC_API_KEY) | "login" (uses your `claude` login)
model = "opus"

[providers.codex]
kind = "codex"
# model = "gpt-5.5-codex"

[verify]                       # <repo>/lyra.toml can override these
build = "cargo build --workspace"
test  = "cargo test --workspace"
fmt   = "cargo fmt --all -- --check"
```

Each `[providers.<id>]` table needs `kind`; `bin = "..."` and `extra_args = [...]` are optional. A repository can ship a `lyra.toml` with a `[verify]` table to override the verify commands; nothing else can be overridden per repository.

## Stages and guards

| Stage | Agent | What the harness enforces |
|---|---|---|
| S0 Intake | – | the repo path is a git repo, `gh` can read the issue; creates a dedicated worktree and branch; records the base commit |
| S1 Context | read-only | collectors are best effort; the agent only reads and searches |
| S2 Repro | edit | the test lives in its **own file** (`<crate>/tests/issue_<n>.rs`, or a new module file registered with one line); nothing else changes; the harness runs exactly that test and it must **fail with the recorded assertion** (a compile error or "0 tests" is not a reproduction); then it is formatted and committed |
| S3 RCA | read-only | file reading and search only; the harness supplies git history and repro output; afterwards the worktree must be unchanged; result is `supported` or `unresolved` |
| S4 Fix | edit | the test file, its registration, `[dev-dependencies]`, `[[test]]`/`autotests` and `.cargo/config*` are locked; `#[ignore]`/`cfg` gates are rejected |
| S5 Verify | – | `cargo build`, the repro test runs and passes (`1 passed`, not filtered), `cargo test`, `cargo fmt --check`; the verified SHA is recorded |
| S6 Draft PR | – | pushes the verified SHA to `lyra/issue-<n>-<run-id>`, finds or creates a **draft** PR whose head is exactly that SHA; fork-aware; body from a template with a disclosure line; operator git identity, no AI co-author trailers |

Read-only stages use the runtime's own restrictions (Claude `--tools "Read,Grep,Glob"`, Codex `--sandbox read-only`); structured answers go through the runtime's schema flags. Details in [docs/PLAN.md](docs/PLAN.md) §3–§4.

## Retry, cancel, interruption

- **Stage statuses**: `pending → running → done | failed | stopped`. *Failed* is an error (tool, provider, guard, timeout); *stopped* is a deliberate wall (not reproduced, RCA unresolved, verify failed, policy not acknowledged, no push access).
- **Retry from stage N** (button on the run page, `POST /api/runs/{id}/retry/{stage}`, or `lyra retry`): outputs of stages ≥ N move to `attempts/<n>/`, the worktree is restored to the checkpoint before N with `git reset --hard` + `git clean -fd`, the verified commit is cleared when N ≤ Verify, and the pipeline continues from N. After a failed verify, **Retry from S4 Fix** reruns the fix with the verify output attached.
- **Cancel** kills the agent's process tree and marks the run `cancelled`; Retry stays available.
- **Interrupted**: when `lyra serve` starts, any run a previous process left `running` is marked `interrupted`; retry it from its stage.
- **Hang guard**: `run.timeout_minutes` per agent call or verify command. There are no cost or time budgets beyond that.

## Where runs live

```
~/.lyra/
  config.toml
  runs/<run-id>/            <repo>-<issue>-<timestamp>
    run.json  events.jsonl  status, stages, checkpoints, attempts ; append-only event log
    worktree/               branch lyra/issue-<n>-<run-id>
    artifacts/              00-issue.md  10-context/  20-repro/  30-rca/  40-fix/  50-verify/  60-pr/  transcripts/
    attempts/               superseded stage outputs after a retry
```

`LYRA_HOME` moves the whole tree. LYRA never clones repositories; it creates a git worktree of the repository you gave it.

## Dashboard API

`POST /api/runs` (RunRequest JSON → `{id}`), `GET /api/runs`, `GET /api/runs/{id}`, `GET /api/runs/{id}/events?since=N` (SSE, event `run`, id = seq), `POST /api/runs/{id}/cancel`, `POST /api/runs/{id}/retry/{stage}`, `GET /api/runs/{id}/artifacts/{path}`, `GET /api/doctor`, `GET /api/config`. Errors are `{"error": "..."}` with 400 / 404 / 409.

## Limitations of this release

- Rust repositories on GitHub only; one active run at a time; one runtime per run.
- Draft PRs only, never merged. The policy acknowledgment is given when the run is submitted; a retry keeps the original answer.
- The agent runs on your machine without a container sandbox. Edit stages may run `cargo`, which executes build scripts of the target repository: only run LYRA on repositories you are willing to build locally.
- The dashboard has no authentication: keep `server.bind` on `127.0.0.1`.
- No automatic resume or fix-retry loop, no cost display, no multi-model RCA, no web search. See [docs/PLAN.md](docs/PLAN.md) §11 and [docs/RESEARCH.md](docs/RESEARCH.md) for what comes next.

## More

- [docs/PLAN.md](docs/PLAN.md): the design (flow, stages, adapters, state, dashboard, storage, decisions).
- [docs/RESEARCH.md](docs/RESEARCH.md): deferred designs and the survey behind them.
- [SETUP.md](SETUP.md): prerequisites, first run, troubleshooting.
- [AGENTS.md](AGENTS.md): rules for contributors and coding agents working on LYRA.
