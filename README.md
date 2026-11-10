# LYRA

LYRA is a local harness around an off-the-shelf coding agent (Claude Code or Codex CLI). You point it at a repository you have already cloned (Rust via the built-in `rust-cargo` profile, Python via `python-pytest`, JavaScript/TypeScript with Vitest via `js-vitest`) and a GitHub issue; it gathers context, has the agent write a failing regression test in its own file, finds the root cause in a read-only pass, has the agent fix the production code while the test is locked, verifies the fix by actually running the test and the workspace checks, and opens a **draft** PR for the exact commit that passed. Every step is checkpointed, written to disk and streamed live to a dashboard on `localhost`. When it cannot finish (the bug does not reproduce, the root cause stays unresolved, verification fails) it stops with the evidence and a Retry button.

## Flow

```
S0 intake    gh issue view (with comments) → 00-issue.md ; worktree ~/.lyra/runs/<id>/worktree
             branch lyra/issue-<n>-<run-id> from the default branch ; base commit recorded
             CONTRIBUTING / AI-policy excerpts shown on the run page
S1 context   related issues/PRs, git log -S, blame, cargo metadata; full tracked root/nested instructions and policies
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
powershell -ExecutionPolicy Bypass -File .\setup.ps1   # Windows; policy applies to this process only
./setup.sh           # Linux / macOS
```

The setup script checks your tools, builds `target/release/lyra` with `--locked`, and runs `lyra doctor`, which creates `~/.lyra/config.toml` if it does not exist. Full prerequisites and troubleshooting are in [SETUP.md](SETUP.md).

```sh
lyra doctor          # runtime + local auth status, gh auth status, git, profile tools (cargo, rustfmt, python3)
lyra serve           # dashboard on http://127.0.0.1:7700 (opens the browser)
```

In the browser: enter the path of a cloned Rust repository, the issue (`https://github.com/owner/repo/issues/123`, `#123` or `123`), pick the runtime, tick the policy box if the repository has a contribution / AI policy you have read, and press **Run**. The run page shows the stage strip, the agent's live log (tool calls, text, file edits), and any error. Artifact links, including `policy.md`, appear as files are written so you can read them while the run continues.

From a terminal instead:

```sh
lyra run --repo /path/to/cloned/repo --issue https://github.com/owner/repo/issues/123
lyra run --repo . --issue 42 --issue-file issue.md     # offline: issue text from a file
lyra run --repo . --issue 42 --profile python-pytest    # choose the profile in a repo that matches several
lyra runs                                              # list runs
lyra retry <run-id> --from fix                         # intake|context|repro|rca|fix|verify|pr
lyra retry <run-id> --from pr --ack-policy             # acknowledge a discovered policy and retry
lyra config                                            # print the effective configuration
```

The dashboard and CLI share a writer lock per `LYRA_HOME`. Stop `lyra serve` before starting a CLI run or retry; read-only commands such as `lyra runs` remain available. Dashboard live events come from runs started by that dashboard process.

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

Each `[providers.<id>]` table needs `kind`; `bin = "..."` and `extra_args = [...]` are optional. A repository can ship a `lyra.toml` with verify overrides (`[verify]` for `rust-cargo`, `[profiles.<id>.verify]` for any profile) and, for Python, `packages` under `[profiles.python-pytest]` to name its production packages; nothing else can be overridden per repository.

### Execution profiles

Everything LYRA knows about the target repository's toolchain and test runner lives in an execution profile; the stages and guards are runner-neutral. Intake detects the profile from files in the worktree without executing anything. Three profiles are built in and checked in this order, the first match winning: `rust-cargo`, when `Cargo.toml` is at the repository root; `python-pytest`, when the root has pytest evidence (`pytest.ini`, `pytest.toml`, `[tool.pytest...]` in `pyproject.toml`, `[tool:pytest]` in `setup.cfg`, `[pytest]` in `tox.ini`, a `conftest.py`, or pytest declared in `pyproject.toml` / `requirements*.txt`); and `js-vitest`, for a root `package.json` with Vitest (see below). A repository matching more than one is warned about at Intake and can be run with another profile through `lyra run --profile <id>`. The resolved profile (`id`, contract `version`, project directory, evidence) is saved in `run.json` and as `artifacts/profile.json`, and is frozen for the run; Retry from Intake re-resolves it. A repository with no matching profile fails Intake with a message naming what was looked for and the supported profile ids; it is a missing-profile diagnostic, not a language rejection, and other languages are onboarded through new profiles ([issue #4](https://github.com/TechWizard9999/LYRA/issues/4), [docs/PLAN.md](docs/PLAN.md) §13). Runs saved before this field existed are read as `rust-cargo`. `lyra doctor` reports each profile's required tools (`cargo`, `rustfmt` for `rust-cargo`; `python3` for `python-pytest`; `node` for `js-vitest`) next to the profile id; Intake refuses a run whose profile tools are missing.

**Python repositories.** LYRA runs the `python3` on `PATH` (Windows: `python`) and installs nothing, so start `lyra serve` / `lyra run` from an environment that has pytest and the project's dependencies, for example `source .venv/bin/activate`. The regression test is a new `test_issue_<n>.py` module selected by its pytest node id; LYRA runs it with the repository's `addopts` neutralised and the cache provider off, so nothing is written into the checkout. Imports are bound to the checkout under test (`PYTHONPATH` = checkout and `src/`), and verification first checks that the project's packages resolve there, so an editable install of another clone cannot stand in for the fix. Verification then runs the node and `python3 -m pytest`, plus `build` / `fmt` only if configured. The fix may change only declared production code; the root `pyproject.toml`, `setup.cfg`, `setup.py`, `tox.ini` and `requirements*.txt` are frozen (LYRA installs nothing, so a dependency change cannot be verified). Everything else is locked like Cargo's test registration (PLAN §13.6): every test, helper, fixture and data file wherever it lives (also inside `src/`, where only source code may change), `conftest.py`, pytest configuration, plugin entry points, docs and CI files. Existing tests cannot be updated by a Python fix. Inputs from outside the repository (network, environment, files elsewhere) are not covered by the lock. Production code is what the repository declares by name: the packages in `pyproject.toml` / `setup.cfg`, or `packages = ["mypkg"]` under `[profiles.python-pytest]` in the repository's `lyra.toml`, wherever they live (the root, `src/`, `lib/`). A `[project] name` counts only when a package or module spelled like it exists. `src/` alone and wildcard-only patterns such as `include = ["*"]` declare nothing. A repository that declares nothing is stopped with a message naming the undeclared files. A repository that matches both profiles uses `rust-cargo` unless you pick one with `--profile` or the dashboard's *Execution profile* select; the run log names the other match.

**JavaScript/TypeScript repositories (Vitest).** LYRA runs the `node` on `PATH` and installs nothing: install the repository's dependencies first, on the default branch (`npm ci`, `pnpm install --frozen-lockfile` or `yarn install --frozen-lockfile`); Vitest 3.x is supported. Intake refuses an install whose `package.json` or lockfile differs from the commit under test, then copies that `node_modules` next to the run's worktree (and next to each verification checkout), where Node and Vite find it. The recorder keeps its Vite cache in the run environment. An overridden Verify command uses its own cache settings, and Verify rejects generated files left inside the checkout. Each checkout gets its own copy. Package links must stay inside the installed dependency tree. The regression test is a new `*.test.<ext>` file selected by its exact path and full name (`describe > test`); LYRA runs it through Vitest's Node API and reads a separate structured report. A Vitest configuration outside the default location is named with `config = "config/unit.ts"` under `[profiles.js-vitest]` in the repository's `lyra.toml`. Production code is what the repository declares (`files`, `main`, `exports` in `package.json`, `include`/`rootDir` in `tsconfig.json`, or `sources = ["src"]` under `[profiles.js-vitest]` in its `lyra.toml`), each at its own scope; a fix may change only source files there. Tests, setup files, mocks, snapshots, fixtures, every `package.json`, lockfiles, `tsconfig` and Vitest/Vite configuration are locked (PLAN §13.8). Workspaces (monorepos) and Jest are not supported yet.

**Verify commands per profile.** The top-level `[verify]` table applies to `rust-cargo` repositories; other profiles use `[profiles.<id>.verify]` (for example `[profiles.python-pytest.verify]` with `fmt = "ruff format --check ."`) or their built-in defaults. A repository's own `lyra.toml` overrides either with the same tables: its `[profiles.<id>.verify]` for that profile, and its top-level `[verify]` for `rust-cargo` only, so a repository with both Cargo and pytest never runs its Cargo commands as the Python suite.

## Stages and guards

| Stage | Agent | What the harness enforces |
|---|---|---|
| S0 Intake | – | the repo path is a git repo, `gh` can read the issue; creates a dedicated worktree and branch; records the base commit |
| S1 Context | read-only | related-issue/history/metadata evidence is best effort; complete tracked root and nested instruction, contribution, AI-use and provenance policies are required and saved with their scope in `10-context/instructions.md`; a policy collection error fails visibly; the agent only reads and searches |
| S2 Repro | edit | the test lives in its **own file** (`<crate>/tests/issue_<n>.rs`, or a new module file registered with one line); nothing else changes; the harness runs exactly that test and it must **fail with the recorded assertion** (a compile error or "0 tests" is not a reproduction); discovery must stop finding that test when its new source file is temporarily emptied; then it is committed |
| S3 RCA | read-only | file reading and search only; the harness supplies git history and repro output; afterwards the worktree must be unchanged; result is `supported` or `unresolved` |
| S4 Fix | edit | the test file, its registration, `[dev-dependencies]`, `[[test]]`/`autotests` and `.cargo/config*` are locked; `#[ignore]`/`cfg` gates are rejected |
| S5 Verify | – | repeats the source-file discovery check, then `cargo build`, the repro test runs and passes (`1 passed`, not filtered), `cargo test`, `cargo fmt --check`; the verified SHA is recorded |
| S6 Draft PR | – | checks existing open PRs for the branch before pushing and refuses to update a non-draft PR; pushes the verified SHA to `lyra/issue-<n>-<run-id>`, finds or creates a **draft** PR whose head is exactly that SHA; fork-aware; body from a template with a disclosure line; operator git identity, no AI co-author trailers |

Read-only stages use the runtime's own restrictions (Claude `--tools "Read,Grep,Glob"`, Codex `--sandbox read-only`); structured answers go through the runtime's schema flags. Details in [docs/PLAN.md](docs/PLAN.md) §3–§4.

## Retry, cancel, interruption

- **Stage statuses**: `pending → running → done | failed | stopped`. *Failed* is an error (tool, provider, guard, timeout); *stopped* is a deliberate wall (not reproduced, RCA unresolved, verify failed, policy not acknowledged, no push access).
- **Retry from stage N** (button on the run page, `POST /api/runs/{id}/retry/{stage}`, or `lyra retry`): every preceding stage must have completed successfully; retries cannot skip an earlier failed, stopped or pending stage. Outputs of stages ≥ N move to `attempts/<n>/`, the worktree is restored to the checkpoint before N with `git reset --hard` + `git clean -fdx` (including ignored files), the verified commit is cleared when N ≤ Verify, and the pipeline continues from N. After a failed verify, **Retry from S4 Fix** reruns the fix with the verify output attached.
- **Contribution policy**: if the PR stage stops for an unacknowledged policy, read the full applicable rules in `10-context/instructions.md` alongside intake's `policy.md` excerpts, then tick the run page's policy checkbox before retrying. The CLI equivalent is `lyra retry <run-id> --from pr --ack-policy`. An existing acknowledgment is preserved on later retries. Acknowledgment does not override a repository's rules.
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

`POST /api/runs` (RunRequest JSON → `{id}`), `GET /api/runs`, `GET /api/runs/{id}`, `GET /api/runs/{id}/events?since=N` (SSE, event `run`, id = seq), `POST /api/runs/{id}/cancel`, `POST /api/runs/{id}/retry/{stage}`, `GET /api/runs/{id}/artifacts/{path}`, `POST /api/doctor`, `GET /api/config`. Errors include 400 / 403 / 404 / 409.

Retry requests may omit the body or send JSON such as `{"policy_acknowledged":true}` to acknowledge the discovered policy before retrying. Omitting the flag preserves any previous acknowledgment. `lyra doctor` checks the selected runtime's local login status or configured API key; credentials are not remotely validated and no model prompt is sent. Missing optional runtimes do not fail the report.

The server accepts loopback `Host` headers on its listening port and rejects cross-site browser requests. Every POST needs an `Origin` matching its URL's scheme, host and port; HTTP's default port 80 may be omitted or written explicitly. For example: `curl -X POST -H "Origin: http://127.0.0.1:7700" http://127.0.0.1:7700/api/doctor`. Browser forms supply this automatically. Health diagnostics only run on this guarded POST; GET does not launch subprocesses.

SSE reconnects resume with `Last-Event-ID`. After all saved events for a terminal, inactive run have been sent, the server sends an `end` event and closes the stream; clients should close their `EventSource` on `end`. A retry starts a new stream when its run page reloads. Artifacts are served as plain text with content sniffing disabled, and paths through symlinks or junctions are rejected.

## Limitations of this release

- GitHub-hosted repositories matching a built-in execution profile: `rust-cargo` (`Cargo.toml` at the repository root) `python-pytest` (pytest evidence at the root) or `js-vitest` (a root `package.json` with Vitest). Python and JavaScript support need a pre-provisioned environment; LYRA does not create virtual environments or install dependencies, and a repro test may not add dependencies. Projects in a subdirectory, unittest-only Python projects, JavaScript workspaces, Jest and other targets are not supported yet. They onboard through new profiles behind the same contract ([issue #4](https://github.com/TechWizard9999/LYRA/issues/4), [docs/PLAN.md](docs/PLAN.md) §13). One active run at a time; one runtime per run.
- Draft PRs only, never merged.
- LYRA builds and tests repositories on your machine without a container sandbox. The harness runs `cargo`, which executes the target repository's build scripts and tests; only run LYRA on repositories you are willing to build locally.
- The dashboard supports loopback addresses only (`127.0.0.1`, `localhost`, or `::1`). Non-loopback binds are rejected; remote hosting is outside this release.
- No automatic resume or fix-retry loop, no cost display, no multi-model RCA, no web search. See [docs/PLAN.md](docs/PLAN.md) §11 and [docs/RESEARCH.md](docs/RESEARCH.md) for what comes next.

## More

- [docs/PLAN.md](docs/PLAN.md): the design (flow, stages, adapters, state, dashboard, storage, decisions).
- [docs/RESEARCH.md](docs/RESEARCH.md): deferred designs and the survey behind them.
- [SETUP.md](SETUP.md): prerequisites, first run, troubleshooting.
- [AGENTS.md](AGENTS.md): rules for contributors and coding agents working on LYRA.

Dashboard browser behavior has offline tests using Node's built-in runner: `node --test src/web/static/app.test.js`. Rust handler and SSE tests run with `cargo test` and need no installed agent CLI or network access.
