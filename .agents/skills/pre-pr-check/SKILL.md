---
name: pre-pr-check
description: Run the full local quality gate (fmt, clippy with warnings denied, cargo test, JS tests, agent-folder sync check) and a final self-review before committing or opening a PR. Use before every commit that matters and every PR.
---

# Pre-PR check

## Gate (run in this order; stop at the first failure and fix it)
```sh
cargo fmt --all                                  # then:
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
# if you touched a profile, its provisioned runner tests too:
cargo test --lib python_pytest -- --ignored          # needs pytest on PATH
LYRA_TEST_NODE_MODULES=<node_modules with vitest> cargo test --lib js_vitest -- --ignored
node --test src/web/static/app.test.js
./scripts/agents.sh check                        # PowerShell: .\scripts\agents.ps1 check
```
On Windows, use PowerShell or cmd for cargo, not Git Bash (`.agents/rules/windows.md`).

If you only edited `.agents/`, the last command is the one that matters (run `sync` first). If you only edited docs, still run `check`.

## Self-review checklist
- [ ] Pipeline order, guards, read-only stages, and draft-only PR behaviour unchanged (or the change is the point and PLAN.md is updated).
- [ ] New behaviour has tests; a bug fix has a test that failed before.
- [ ] No new dependency without a reason ready for the PR; `Cargo.lock` is committed if it changed.
- [ ] Contract module touched? PR note + `change-contract` checklist done.
- [ ] Prompts/schemas edited only in `src/prompts` / `src/schemas`.
- [ ] No secrets, absolute home paths, or large transcripts in the diff (`git diff --stat`, then skim).
- [ ] README / SETUP / PLAN updated for user-visible or design changes.
- [ ] Commit message has no AI co-author trailer.

## If something fails
- fmt: run `cargo fmt --all`; don't hand-edit around `rustfmt.toml`.
- clippy: fix the code; an `#[allow]` needs a reason comment.
- a test fails: don't weaken or delete it. Find out whether the test or the code is wrong; ask if unsure.
- `agents.sh check` reports drift: run `sync`, review the diff, commit `.claude/skills/`.

Report results honestly: say which commands ran and which failed, with the key line of output.
