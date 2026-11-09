---
name: onboard
description: Get a new developer (or agent) productive on LYRA - prerequisites, build, doctor, first test run, architecture tour, and where the rules live. Use when someone is new to the repo or sets up a fresh machine.
---

# Onboard to LYRA

## 1. What LYRA is (2 minutes)
A Rust harness around Claude Code or Codex. Input: a path to an already-cloned repository (Rust today, through the built-in `rust-cargo` execution profile; PLAN §13) plus a GitHub issue. It runs one fixed pipeline, intake → context → repro → rca → fix → verify → draft PR, enforces guards between stages, saves everything under `~/.lyra/runs/<id>/`, and streams to a localhost dashboard. Read `README.md` (flow + stage table), then skim `docs/PLAN.md` §1–§3.

## 2. Set up
Prerequisites and troubleshooting are in `SETUP.md`. Short version: Rust stable (`rustup`), `git`, GitHub CLI authenticated (`gh auth login`), Node (only for the dashboard JS tests), and at least one runtime (`claude` and/or `codex`).
```sh
./setup.sh                                  # Windows: powershell -ExecutionPolicy Bypass -File .\setup.ps1
./target/release/lyra doctor
./scripts/agents.sh install-hooks           # Windows: .\scripts\agents.ps1 install-hooks
./scripts/agents.sh sync                    # make sure .claude/skills is current
```
Windows: use PowerShell for cargo (`.agents/rules/windows.md`).

## 3. Prove your environment
```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
node --test src/web/static/app.test.js
```
All must pass on a clean checkout with no network, `gh` login, or agent CLI. If not, fix your setup before changing code.

## 4. Architecture tour (read in this order)
1. `src/lib.rs`: module map.
2. `src/events.rs`, `src/state.rs`, `src/error.rs`: the vocabulary (`StageId`, `RunState`, `StageOutcome`, `StopReason`).
3. `src/engine.rs`: how a run is driven, retried, cancelled.
4. `src/stages/mod.rs` then `repro.rs`: the shape of a stage and the most guarded one.
5. `src/guards.rs` (universal rules) and `src/profile/` (the `ExecutionProfile` contract and the `rust-cargo` profile, where Cargo-specific rules live): the rules that make results trustworthy.
6. `src/providers/mod.rs` and `claude.rs`: how an agent is called and isolated.
7. `src/web/server.rs`: request guards and the dashboard.
8. `src/prompts/` and `src/schemas/`: what we ask agents and the shape we require.

## 5. Where rules and playbooks are
- `AGENTS.md`: canonical rules and definition of done.
- `.agents/rules/`: development, testing, contracts, security, git-and-prs, windows.
- `.agents/skills/`: pick the one matching your task (`change-a-stage`, `add-guard`, `add-provider-adapter`, `add-prompt-or-schema`, `add-artifact`, `change-contract`, `add-dashboard-feature`, `write-tests`, `record-fixture`, `pre-pr-check`, `acceptance-run`, `debug-a-run`, `commit-and-pr`, `review-checklist`, `update-docs`).

## 6. First contribution suggestion
Pick a small guard edge case or a missing test, write the failing test first (`write-tests`), fix, run `pre-pr-check`, then `commit-and-pr`.

## Things new people trip on
- No AI co-author trailers in commits; your own git identity only.
- LYRA never clones; tests never touch the real `~/.lyra`.
- CLI runs and `lyra serve` share a writer lock: stop one before using the other.
- Contract modules need a PR note.
