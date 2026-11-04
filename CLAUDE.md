# CLAUDE.md

See [AGENTS.md](AGENTS.md): it is the canonical guide (purpose, architecture map, rules, tests, definition of done). The ten rules that matter most:

1. Rust 2021; `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` clean; unit tests in every module (`#[cfg(test)]`).
2. Keep the fixed pipeline: intake → context → repro → rca → fix → verify → draft PR. No skipped stages, no alternative flows.
3. No cost or time budgets; only the hang-guard `run.timeout_minutes` and Cancel.
4. LYRA never clones repositories; it takes a path to an already-cloned repo and uses its own worktree.
5. Never auto-merge, never open non-draft PRs; the PR is for the exact verified commit.
6. No AI co-author trailers in commits; commits use the operator's git identity.
7. No new dependencies without a reason in the PR description.
8. Contracts (`error`, `events`, `state`, `config`, `artifacts`, `proc`, `git`, `providers/mod.rs`, `stages/mod.rs`, `engine`) change only with a note in the PR.
9. Prompts live in `src/prompts/`, schemas in `src/schemas/`; artifacts follow the PLAN §7 layout.
10. On Windows, run cargo from PowerShell, not Git Bash (its `link` shadows the linker).
