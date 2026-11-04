---
name: add-artifact
description: Add a new file a stage writes into a run's artifacts directory, following the PLAN section 7 layout and announcing it with an artifact_written event. Use when a stage needs to save new evidence or output.
---

# Add an artifact

## Layout (PLAN §7)
```
~/.lyra/runs/<run-id>/
  run.json  events.jsonl
  worktree/
  artifacts/  00-issue.md  10-context/  20-repro/  30-rca/  40-fix/  50-verify/  60-pr/  transcripts/
  attempts/   superseded stage outputs after a retry
```
`RunDir::stage_dir(StageId)` gives the right `<NN-stage>` directory.

## Steps
1. Choose the stage directory that produces it. Don't create new top-level directories or write outside `artifacts/` (except `worktree/` content that is part of the repo change).
2. Write through `RunDir`: `write(rel, bytes)`, `write_json(rel, &value)`. Never build paths by hand or write with `std::fs` directly.
3. Emit an `artifact_written` event so the dashboard lists it live (see how existing stages do it through the `EventSink`).
4. Make it retry-safe: the file must live where `RunDir::archive_from(stage, n)` will move it on retry, so a rerun starts clean and old output goes to `attempts/<n>/`.
5. Bounded size: truncate large command output with an explicit marker. Truncated output must never be treated as success evidence.
6. No secrets: don't store environment values, tokens, or absolute home paths.
7. If another stage reads it, read it as untrusted-if-model-written and validate shape (and paths inside it) before use. If it holds a decision the pipeline depends on, record it in `run.json` instead (a contract change; use `change-contract`) and treat the artifact as display only, like `repro.json` vs `frozen_repro`.
8. Test: written under the right directory, appears in `RunDir::list()`, event emitted, moved by `archive_from` on retry.
9. If the PR body or dashboard should surface it, update `stages/pr.rs`/`web/views.rs` accordingly. Update PLAN §3 (outputs table) and README's run layout if you added a user-visible file.
