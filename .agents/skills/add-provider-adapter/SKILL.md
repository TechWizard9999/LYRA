---
name: add-provider-adapter
description: Add or modify an agent runtime adapter (Claude Code, Codex, or a new CLI) behind the Agent trait. Use when touching src/providers/, provider config, or runtime flags.
---

# Add or change a provider adapter

Note: `providers/mod.rs` is a contract; read `.agents/rules/contracts.md`. More runtimes (Copilot CLI, OpenCode, generic command) are deferred designs in `docs/RESEARCH.md` §1. A new runtime is a scoped feature: confirm with maintainers first, since PLAN §1 ships two.

## Read first
- `docs/PLAN.md` §4 (adapter table) and `.agents/rules/security.md` (agent isolation).
- `src/providers/mod.rs` (`Agent`, `AgentTask`, `AgentOutput`, `Mode`, `make_agent`), then `claude.rs` or `codex.rs` as the model to copy.
- `src/config.rs` (`ProviderKind`, `ProviderConfig`) and `src/doctor.rs`.

## What an adapter must do
- Implement `Agent::{id, describe, run}`. `run` forwards normalised `AgentEvent`s to the `EventSink` under `task.stage` and returns `AgentOutput` (`ok`, `failure`, `structured`, `timed_out`, `cancelled`, `exit_code`, `transcript`).
- Spawn only through `proc` (hang guard, cancellation, process-tree kill, Windows `.cmd` shims). Env comes from `scrubbed_env` plus `provider_env(<only this provider's keys>)`.
- `Mode::ReadOnly`: file reading and search only, no command tools, no edits. `Mode::Edit`: file edits only; the harness runs commands.
- Disable user settings, hooks, MCP, web, and shell tools by flags; if the installed CLI does not support an isolation flag, **fail visibly**, never run without it.
- Never grant the artifacts dir as a workspace (`--add-dir`).
- Deliver the schema through the runtime's flag, then **re-validate** with `jsonschema` against `src/schemas/`; don't trust the flag. A JSON-looking free-text fallback must not bypass required fields.
- Allowlist `extra_args` narrowly; reject anything touching tools, settings, sandbox, auth, paths.
- Write the raw JSONL transcript (capped at 8 MiB per attempt, with a truncation marker; transcript I/O failure fails the call).
- Hide internal tool calls (e.g. Claude's `StructuredOutput`) from the live log.

## Steps
1. Capture a real output sample and add it as a fixture (`record-fixture`).
2. Write parser tests against the fixture first: text, tool call, tool result, file change, success, failure (permission denial, `turn.failed`, missing or invalid structured output), and a truncated variant.
3. Implement the adapter and a `fake_binary` test helper for both Windows (`.cmd`) and Unix (`.sh`), following the existing ones.
4. Register in `make_agent` and `ProviderKind`; add config defaults and keep old `config.toml` files loading.
5. Add a `doctor` check: binary on PATH, version, noninteractive login status or "API key configured". Never send a model prompt or print key values.
6. Update README (Configuration), SETUP.md (prerequisites), PLAN §4 (`update-docs`).

## Verify
`pre-pr-check`. Real-runtime behaviour needs an `acceptance-run`; say in the PR whether you did one.
