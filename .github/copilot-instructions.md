# Copilot instructions

Follow [AGENTS.md](../AGENTS.md); it is the canonical guide for this repository. The rules:

1. Rust 2021, stable toolchain; `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` clean.
2. Unit tests in every module (`#[cfg(test)] mod tests`); tests need no network, `gh` auth or installed runtime.
3. No new dependencies without a reason in the PR description.
4. Keep the fixed pipeline: intake → context → repro → rca → fix → verify → draft PR.
5. No cost or time budgets; only the hang-guard `run.timeout_minutes` and Cancel.
6. LYRA never clones repositories; it takes a path to an already-cloned repo and uses its own worktree.
7. Never auto-merge, never non-draft PRs; the PR is for the exact verified commit.
8. No AI co-author trailers in commits; commits use the operator's git identity.
9. On Windows, run cargo from PowerShell (Git Bash's `link` shadows the linker).
10. Contracts (`error`, `events`, `state`, `config`, `artifacts`, `proc`, `git`, `providers/mod.rs`, `stages/mod.rs`, `engine`) change only with a note in the PR.
11. Prompts live in `src/prompts/`, schemas in `src/schemas/`; artifacts follow the PLAN §7 layout (`artifacts/<NN-stage>/...`).
12. Read-only stages stay read-only; guards are not weakened to make a run pass.
13. Done means: builds, fmt/clippy/tests clean, new behaviour tested, docs updated for user-visible changes.
14. Detailed rules and skills live in `.agents/` (see `.agents/README.md`).
