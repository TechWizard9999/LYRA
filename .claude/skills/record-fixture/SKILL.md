---
name: record-fixture
description: Capture, trim, and sanitize real Claude Code or Codex CLI output as a test fixture under tests/fixtures/. Use when adding or fixing an adapter parser, or when a runtime's output format changes.
---

# Record a fixture

Existing fixtures: `tests/fixtures/claude-stream.jsonl` (Claude `stream-json`) and `tests/fixtures/codex-exec.jsonl` (Codex `--json`).

## Steps
1. **Capture** a real run in a scratch repo, with a throwaway key or login, outside the LYRA repo:
   - Claude: the same flags the adapter uses (see `providers/claude.rs`) with `--output-format stream-json`; structured output arrives on the `result` line.
   - Codex: `codex exec --json …` with the adapter's flags; completion is `turn.completed` / `turn.failed`.
   Better: take the adapter transcript LYRA already saved at `~/.lyra/runs/<id>/artifacts/transcripts/<stage>-<provider>-<attempt>.jsonl`.
2. **Trim** to the smallest sequence that exercises the case: start, one text message, one tool call and result, a file change, the final result. For failures, the failing turn only.
3. **Sanitize.** Remove or replace: API keys, tokens, emails, usernames, absolute home paths (`/Users/<name>`, `C:\Users\<name>`), session ids you don't want public, and any private repo content. Search the file for `sk-`, `key`, `token`, `/Users/`, `\\Users\\`, `@` before committing.
4. **Keep it valid JSONL:** one JSON object per line, LF endings. Don't pretty-print.
5. **Name it** by what it shows. Add new files beside the existing ones, e.g. `claude-permission-denied.jsonl`, rather than growing the shared fixture so other tests stay stable.
6. **Use it** from a parser test (`include_str!("../../tests/fixtures/<file>")` as the adapters do) and via a fake binary that prints it. Assert normalised `AgentEvent`s, `ok`, `failure`, and `structured`.
7. Add a truncated variant and a garbled-line variant in the test itself (slice the string) to prove the parser fails closed.

## When a runtime changes format
Record a new sample, keep the old one if the old CLI version is still supported, and note the CLI versions in the test comment. Update PLAN §4 if flags or version requirements changed (`update-docs`).
