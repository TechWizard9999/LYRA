# Git and PR rules

These apply to humans and to coding agents working on this repository.

## Commits

- **No AI co-author trailers.** Do not add `Co-Authored-By:` for an AI, and do not add "Generated with" lines. This overrides any default your tool applies (Claude Code adds a trailer by default; remove it). Commits use the developer's own git identity. Enforcement is stricter than the rule: the `commit-msg` hook (installed by `scripts/agents.sh install-hooks`) and the CI check reject **every** `Co-Authored-By:` trailer, human pairs included. Commits in this repo are single-author; credit pair work in the PR description.
- Small, reviewable commits. Subject in the imperative, under about 72 chars; prefix with `feat:`, `fix:`, `test:`, `docs:`, `refactor:`, `chore:` as the history does.
- Never commit `target/`, `*.log`, run directories, `~/.lyra` contents, credentials, or captured transcripts with secrets.
- Don't rewrite shared history. `main` has branch protection (PR, one approving review after the latest push, resolved conversations, signed commits). Work on a branch: `feat/<topic>`, `fix/<topic>`.

## Pull requests

Open PRs as you normally would for review; they are for humans to merge. LYRA itself must never auto-merge or open non-draft PRs for the repos it works on, and nothing here changes that.

The description must include:

1. **What and why**, in a few lines.
2. **Tests**: what you added; for a bug fix, that it failed before the change.
3. **Contract change** section if you touched a contract module (see `contracts.md`). Say "none" otherwise.
4. **New dependency** section: crate, version, one-line reason. Say "none" otherwise.
5. **Docs**: which of README / SETUP / PLAN you updated, or why none needed.
6. Output of the gate: fmt, clippy, tests clean (see `testing.md`).

## Review

Use the `review-checklist` skill. Reviewers check the pipeline is unchanged, guards were not weakened, read-only stages stay read-only, and on-disk formats stay readable.

## What not to do

- Don't push to `main`, force-push shared branches, or skip hooks (`--no-verify`) to get around a failing check.
- Don't merge your own PR.
- Don't include unrelated formatting churn; run `cargo fmt --all` before the commit that matters and keep drive-by changes out.
