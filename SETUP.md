# Setting up LYRA

This walks you from a fresh clone to a first run. The short version is `./setup.ps1` (Windows) or `./setup.sh` (Linux / macOS); both are safe to run again.

If Windows execution policy blocks the script, run `powershell -ExecutionPolicy Bypass -File .\setup.ps1`. This sets the policy for that process without changing the machine's policy. The setup script builds with the checked-in lockfile and lets `lyra doctor` create the default config; existing configuration is preserved.

## Prerequisites

| Tool | Why | Install / check |
|---|---|---|
| Rust stable | builds LYRA; LYRA also runs `cargo` on the target repo | [rustup](https://rustup.rs), then `rustup default stable` |
| A linker (Windows) | Rust needs one | either the GNU toolchain: `rustup toolchain install stable-x86_64-pc-windows-gnu` + `rustup default stable-x86_64-pc-windows-gnu` with [WinLibs](https://winlibs.com) gcc (MinGW-w64) on `PATH`; or the MSVC toolchain with **Visual Studio Build Tools** ("Desktop development with C++") |
| git | worktrees, branches, checkpoints | `git --version` |
| GitHub CLI | reads issues, pushes, opens draft PRs | [cli.github.com](https://cli.github.com), then `gh auth login` and `gh auth status` |
| Claude Code and/or Codex CLI | the agent runtime | see below |

At least one runtime:

- **Claude Code**: `npm i -g @anthropic-ai/claude-code`.
  - `auth = "api_key"` (default): set `ANTHROPIC_API_KEY`. LYRA runs `claude --bare`, so your personal settings, hooks and MCP servers are not loaded.
  - `auth = "login"`: run `claude` once and log in. LYRA preserves that login while using restricted/safe mode, disabling hooks and MCP servers, and ignoring user/project customization. Agents get file tools; the harness runs build and test commands.
- **Codex CLI**: `npm i -g @openai/codex`, then `codex login`, or set `CODEX_API_KEY`. Select it with `[run] provider = "codex"` or per run in the dashboard.

## Build

```sh
cargo build --release --locked
```

The binary is `target/release/lyra` (`target\release\lyra.exe` on Windows). Put it on your `PATH` or call it by path. `cargo install --path . --locked` also works.

## Configure

LYRA reads `~/.lyra/config.toml`. Set `LYRA_HOME` to use another directory (config and runs both move). The file is created with defaults the first time you run any `lyra` command (or by the setup script); `lyra config` prints the effective configuration. The keys are described in [README.md](README.md#configuration). A repository may contain a `lyra.toml` with a `[verify]` table to override the build / test / fmt commands.

## Check

```sh
lyra doctor
```

It checks that the configured runtime is on `PATH` and authenticated, that `gh auth status` succeeds, and that `cargo` is present. Fix every line marked `!!` before the first run. The same report is available at `POST /api/doctor` with a matching Origin header, for example `curl -X POST -H "Origin: http://127.0.0.1:7700" http://127.0.0.1:7700/api/doctor`.

## First run

1. Clone the repository you want to fix and make sure it builds: `git clone https://github.com/owner/repo && cd repo && cargo build`. LYRA never clones; it works in its own git worktree of that clone, on a new branch `lyra/issue-<n>-<run-id>`, so your checkout is left alone.
2. Start the dashboard: `lyra serve`. It prints the URL (`http://127.0.0.1:7700` by default) and opens the browser (`[server] open_browser = false` turns that off).
3. Fill in the repository path and the issue (URL, `#123` or `123`; or paste the issue text to run without GitHub access), pick the runtime, tick the policy box if you read the repository's contribution / AI policy, press **Run**.
4. Watch the run page: each stage turns blue while running and green when done; the live log shows the agent's tool calls (`[Read] src/x.rs`), text and file edits. Artifacts appear as they are written.
5. Outcome: either **Open PR** (a draft PR whose head is the verified commit), or a stopped / failed stage with the message, a details link and **Retry from <stage>** buttons.

The same from a terminal: `lyra run --repo /path/to/repo --issue <url>` prints the events and exits with 0 (done), 3 (stopped), 4 (stage failed) or 5 (cancelled / interrupted).

Use one writer process per `LYRA_HOME`: stop the dashboard before running CLI `run` or `retry`. A writer lock prevents a second process from interrupting or changing a live run. Read-only commands such as `lyra runs` can still inspect saved runs.

## Troubleshooting

- **`link.exe` not found** (Windows, MSVC toolchain without Build Tools): install Visual Studio Build Tools, or switch to the GNU toolchain (`rustup default stable-x86_64-pc-windows-gnu`) with WinLibs gcc on `PATH`.
- **Linker errors only from Git Bash** (Windows): Git Bash puts its own `link` (coreutils) ahead of the real linker on `PATH`. Run `cargo` from PowerShell or cmd.
- **`gh` not authenticated**: intake fails with the `gh` message. Run `gh auth login`, check `gh auth status`, and make sure the account can read the issue and push to the repo or a fork. With no push rights the PR stage stops and shows the `gh repo fork` command to run.
- **`claude` / `codex` not found on Windows**: npm installs `.cmd` shims (`claude.cmd`, `codex.cmd`). LYRA resolves them through `PATH`; check `where claude`. If they are elsewhere, set `bin = "C:\\path\\to\\claude.cmd"` in the provider table.
- **Auth errors from the runtime**: `auth = "api_key"` needs `ANTHROPIC_API_KEY` in the environment `lyra serve` was started from; `auth = "login"` needs a prior `claude` login. For Codex use `codex login` or `CODEX_API_KEY`.
- **A stage fails with a timeout**: an agent call or verify command ran longer than `run.timeout_minutes` (default 30). Large workspaces may need more; raise it and press **Retry from <stage>**. It is a hang guard, not a budget.
- **"a run is already active"**: one run at a time. Wait for it, or press **Cancel** on its page.
- **A run shows `interrupted`**: the previous `lyra serve` / `lyra run` process ended while it was running. Retry it from the stage shown.
- **Port 7700 in use**: change `[server] bind` to another loopback address and port, for example `127.0.0.1:7701`. The dashboard refuses LAN/wildcard binds such as `0.0.0.0:7700`.
- **API request returns 403**: use the loopback dashboard URL and send its exact origin (for example `Origin: http://127.0.0.1:7700`) on POST requests. Cross-site browser requests and unexpected Host headers are rejected.
