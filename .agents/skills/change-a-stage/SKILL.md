---
name: change-a-stage
description: Change the behaviour of one pipeline stage (intake, context, repro, rca, fix, verify, pr) safely. Use when editing anything under src/stages/ or the prompt, schema, or guard a stage depends on.
---

# Change a stage

## Read first
- `docs/PLAN.md` §3 for the stage you are changing (and §3.x for repro/rca/fix/verify/pr).
- `.agents/rules/development.md` and `.agents/rules/security.md`.
- The stage file, its prompt in `src/prompts/`, its schema in `src/schemas/`, its universal guard functions in `src/guards.rs`, and the runner-specific rules in the execution profile (`src/profile/`, `rust-cargo` today).

## Know the model
- Each stage takes `StageCtx` (run dir, mutable `RunState`, config, agent, events, cancel token) and returns `StageOutcome`: `Completed`, `Stopped { reason, message }`, or `Failed { error }`.
- The engine owns sequencing, persistence, retries, and checkpoints. A stage must not advance, skip, or re-order stages.
- Stages record facts in `ctx.state` (base commit, checkpoints `s2`/`s4`, `frozen_repro`, `verified_commit`, PR URL); the engine saves after the stage.
- S1 and S3 call the agent in `Mode::ReadOnly` and must leave the worktree unchanged. S2 and S4 use `Mode::Edit`. The harness runs all commands.
- Later stages read the **frozen** repro record from `run.json`, never the editable `20-repro/repro.json`.

## Steps
1. Decide failed vs stopped. Stopped is only for a deliberate wall (`NotReproduced`, `RcaUnresolved`, `VerifyFailed`, `PolicyNotAcknowledged`, `NoPushAccess`). Everything else is `Failed` with the right `ErrorKind`. A new `StopReason` is a contract change; use `change-contract`.
2. Put rule logic (diff checks, output parsing, matching) in pure functions with tests (`add-guard`), not inline in the stage: language-independent rules in `guards.rs`, anything that names Cargo or a test runner's output in the profile. Stages call the `ExecutionProfile` contract and never name Cargo.
3. Prompt text changes go in `src/prompts/<stage>.md`; keep untrusted input inside `BEGIN/END UNTRUSTED DATA` blocks. Output shape changes go in `src/schemas/<stage>.schema.json` and the Rust type that deserializes it (`add-prompt-or-schema`).
4. New files go through `RunDir` under `artifacts/<NN-stage>/` and emit `artifact_written` (`add-artifact`).
5. Honour cancellation (`ctx.cancel`) and the hang guard (`ctx.timeout()`) for every subprocess or agent call.
6. Make sure retry-from-stage still works: after your change, restoring the previous checkpoint and rerunning this stage must be valid. Anything the stage writes outside `artifacts/<NN-stage>/` or the worktree needs a reason.
7. Tests in the stage module: success, each failure kind, each stop reason, cancellation. Use fake agents and `tempfile` repos (`write-tests`).

## Verify
- `pre-pr-check`.
- If behaviour is visible to users or changes a guard, update `docs/PLAN.md` and README in the same PR (`update-docs`).
- Pipeline order and draft-only PR behaviour must be unchanged.
