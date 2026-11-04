---
name: commit-and-pr
description: Commit and open a pull request for a LYRA change following repo conventions (no AI co-author trailers, operator identity, required PR sections). Use when asked to commit, push, or open a PR.
---

# Commit and PR

Rules: `.agents/rules/git-and-prs.md`. Gate first: `pre-pr-check`.

## Commit
1. `git status` and `git diff --stat`; stage only what belongs to this change. No `target/`, logs, run dirs, secrets, or transcripts.
2. Message: imperative subject, about 72 chars max, with a type prefix (`feat:`, `fix:`, `test:`, `docs:`, `refactor:`, `chore:`). Body explains why when it isn't obvious.
3. **No AI co-author trailer, no "Generated with" line.** If your tool wants to add `Co-Authored-By:`, remove it. This repo's rule beats tool defaults. The identity is the developer's own `git config user.name/email`; don't override it.
4. Don't use `--no-verify` or amend/rewrite commits others have pulled. If a hook fails, fix the cause (the `commit-msg` hook rejects `Co-Authored-By:`; `pre-commit` checks `.claude/skills` sync and fmt).
5. Skills changed? `./scripts/agents.sh sync` and commit `.claude/skills/` in the same commit.

## Branch and push
Work on `feat/<topic>` or `fix/<topic>`, never `main` (protected: PR, one approval after the latest push, resolved conversations, signed commits). Push only when asked.

## PR description template
```
## What and why
<few lines>

## Tests
<added/changed; for a bug fix, that it failed before the change>

## Contract change
<none | module(s), what changed, affected parties, compatibility (see change-contract)>

## New dependency
<none | crate, version, one-line reason>

## Docs
<README / SETUP / PLAN updated, or why not needed>

## Checks
fmt, clippy -D warnings, cargo test, node tests: <results>
```
Open the PR for human review. Never merge it yourself. A PR from a coding agent should say so only if the team's policy asks for disclosure, and must not add a co-author trailer to commits.

## After opening
Respond to review by pushing new commits (not force-pushing over reviewed history unless asked); new pushes dismiss stale approvals, which is expected.
