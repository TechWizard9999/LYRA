---
name: update-docs
description: Decide which docs to update for a change (README, SETUP, PLAN, AGENTS, .agents) and keep them consistent. Use when behaviour, config, CLI, API, setup steps, or design changes, or when agent rules/skills change.
---

# Update docs

| Change | Update |
|---|---|
| New/changed CLI command or flag | `README.md` (Quick start / CLI block), `src/main.rs` help text |
| Config key, default, or `lyra.toml` override | `README.md` Configuration, `docs/PLAN.md` §7, `Config::ensure_default` text |
| Stage behaviour, guard rule, lock, verify step | `docs/PLAN.md` §3.x and the stage/guard table in `README.md` |
| Adapter flags, runtime versions, isolation | `docs/PLAN.md` §4, `SETUP.md` prerequisites, `README.md` Limitations if scope changed |
| Dashboard routes, SSE, request guards | `README.md` Dashboard API, `docs/PLAN.md` §6 |
| State/events/artifact layout or compat rules | `docs/PLAN.md` §5, §7, §12; `README.md` Where runs live |
| Setup steps, prerequisites, troubleshooting | `SETUP.md`, `setup.sh`, `setup.ps1` |
| A deferred idea becomes real or is dropped | `docs/PLAN.md` §11 and `docs/RESEARCH.md` |
| New design decision (product) | `docs/PLAN.md` §10 |
| New working-practice decision | `.agents/decisions/NNNN-*.md` |
| A rule for contributors/agents | `.agents/rules/*.md`; if it's one of the ten top rules, also `AGENTS.md` and `CLAUDE.md` |
| A skill added/changed | `.agents/skills/<name>/SKILL.md`, the tables in `.agents/README.md`, then `scripts/agents.sh sync` |

## Principles
- Docs describe what the code does now. If code and PLAN disagree, fix whichever is wrong in the same PR.
- PLAN.md changes belong in the same PR as any pipeline or guard change (AGENTS.md "done" criteria).
- Link instead of copy. `AGENTS.md` is canonical for rules; skills and rules link to PLAN sections rather than restating them.
- Keep it specific and short. Show exact commands, paths, and flag names.
- Don't document secrets, personal paths, or machine-specific setup.
- Update `src/lib.rs`'s module map if you add or remove a top-level module.

## Check
Re-read the changed doc as a new developer would. Run any commands you documented. Then `pre-pr-check`.
