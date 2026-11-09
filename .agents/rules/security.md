# Security rules

LYRA runs a coding agent and the target repo's build scripts and tests on the developer's machine. It is **not a container sandbox**; do not claim or imply it is. The controls below are what we do have. Keep them intact.

## Trust boundaries

- **Untrusted:** issue and comment text, target-repo files (including its AGENTS.md / CONTRIBUTING), tool output, and every field the agent returns (paths, symbols, commands, test names).
- **Trusted:** the harness code, bundled prompts and schemas, the operator's config.
- Prompts mark untrusted input in `BEGIN/END UNTRUSTED DATA` blocks. Keep that pattern for any new prompt input.

## Agent isolation (`providers/`)

- Environment is scrubbed (`proc::scrubbed_env`, `provider_env`): only that provider's credentials and toolchain necessities.
- Claude: native `Read,Grep,Glob` (+ `Edit,Write` in edit mode), no Bash/MCP/browser tools, hooks and user settings disabled, strict empty MCP config. Codex: sandbox flags, shell tool disabled, user config ignored.
- `extra_args` are allowlisted. Never allow config to override tools, settings, sandbox, auth, or paths. Unsupported isolation flags must fail visibly, not be dropped.
- Never grant the artifacts directory as an agent workspace (no `--add-dir`).
- Structured output is re-validated against `src/schemas/` by the harness; a JSON-looking free-text fallback must not bypass required fields or enums.

## Paths and files

- Validate model-supplied paths before use: reject empty, absolute, `..`, drive prefixes (`:`), `.git`, and anything whose canonical form leaves the worktree (see `safe_source` in `stages/mod.rs`). Use `dunce::canonicalize`.
- Artifact serving rejects symlinks and junctions and sends `nosniff`. Do not follow links when listing or reading run files.
- Run ids are validated before any filesystem access (`engine::run_dir`).

## Read-only stages

S1 and S3 must leave the worktree unchanged: compare complete before/after snapshots (including ignored-file contents) and HEAD. A change fails the stage. Never relax this to speed up a run.

## Dashboard (`web/`)

- Loopback binds only. Validate `Host` on every request (DNS rebinding) and require a matching `Origin` on every POST. Honour `Sec-Fetch-Site`.
- `GET` handlers never launch subprocesses. Health checks (`doctor`) only run on the guarded POST.
- Filesystem/git work in handlers runs through blocking workers.
- Public text neutralizes `@mentions` (PR body, comments).

## Secrets

- Never log, save in artifacts, or put in a PR body any environment variable values, tokens, or `ANTHROPIC_API_KEY` / `CODEX_API_KEY` / `OPENAI_API_KEY`. Doctor reports "configured", not the value.
- Test fixtures must not contain real keys, emails, or absolute home paths.

## Publishing

Draft PRs only; PR head equals the verified commit; never update a PR that is already ready for review; fork-aware pushes; policy acknowledgement is a human action recorded under the writer lock. See `git-and-prs.md`.

## Reporting

If you find a way around a control above, do not ship a workaround; open it as a security issue with the maintainers and propose the fix with a test.
