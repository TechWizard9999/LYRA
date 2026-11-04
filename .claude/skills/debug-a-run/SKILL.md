---
name: debug-a-run
description: Diagnose a failed, stopped, cancelled, or interrupted LYRA run by reading run.json, events.jsonl, artifacts, and agent transcripts. Use when a run did not finish or produced something unexpected.
---

# Debug a run

All data is on disk under `$LYRA_HOME/runs/<run-id>/` (default `~/.lyra/runs/<run-id>/`). The run id looks like `<repo>-<issue>-<timestamp>`.

## 1. Orient
```sh
lyra runs                               # status of every run
lyra config                             # effective configuration
```
Open `run.json`: `status` (`queued|running|stopped|failed|done|interrupted|cancelled`), current `stage`, per-stage status, `base_commit`, `checkpoints` (`s2`, `s4`), `frozen_repro`, `verified_commit`, `attempts[]`, and the last error or stop reason.

## 2. Failed vs stopped
- **failed**: something broke (`ErrorKind`: provider, tool, policy, git, github, io, schema, guard, timeout, config). `retryable` says whether Retry may help. `details` points at a log.
- **stopped**: the harness hit a deliberate wall: `not_reproduced`, `rca_unresolved`, `verify_failed`, `policy_not_acknowledged`, `no_push_access`. The evidence is in that stage's artifacts.

## 3. Find the cause (by stage)
| Stage | Look at |
|---|---|
| intake | `00-issue.md/json`, `policy.md`; `gh auth status`, repo path is a git repo |
| context | `10-context/{related.json,git-history.md,repo.json,instructions.md,summary.json}`; a policy collection error fails visibly |
| repro | `20-repro/repro.json`, `output.txt`, source-binding evidence; was it a compile error, "0 tests", or an unrelated panic (all non-reproductions)? |
| rca | `30-rca/rca.json`; "worktree changed" means the read-only guard tripped |
| fix | `40-fix/{fix.diff,notes.md,scope.json}`; which lock did the guard cite (test file, registration, dev-deps, `[[test]]`, `.cargo/config*`)? |
| verify | `50-verify/{build,repro-after,tests,fmt}.txt`, `source-binding.txt`, `result.json` |
| pr | `60-pr/{body.md,url.txt}`; existing PR state, fork/push access, policy acknowledgement |

## 4. Read the timeline
`events.jsonl` is append-only, one JSON object per line: `run_created`, `stage_started`, `stage_completed`, `artifact_written`, agent events (text, tool calls, file changes, errors), and the terminal event. Tail it to see the last thing that happened; `seq` orders it. A truncated last line after a crash is normal.

## 5. Read the agent
`artifacts/transcripts/<stage>-<provider>-<attempt>.jsonl` is the raw runtime output (capped at 8 MiB per attempt). Look for permission denials, `turn.failed`, missing or invalid structured output, and the model's own explanation.

## 6. After you know the cause
- Environment/transient (auth, network, timeout): fix and **Retry from that stage** (dashboard button, or `lyra retry <id> --from <stage>`). All earlier stages must be `done`.
- Policy stop: read `10-context/instructions.md` and `policy.md`, then `lyra retry <id> --from pr --ack-policy` only if you accept the rules.
- Interrupted: restart `lyra serve` (it marks leftovers `interrupted`), then retry from the recorded stage.
- A real LYRA bug: write a failing test that reproduces it (`write-tests`), never loosen a guard to get past it, and use `change-a-stage` or `add-guard`.

## Notes
- Retry restores the worktree with `git reset --hard <checkpoint>` + `git clean -fdx` and moves later outputs to `attempts/<n>/`; earlier attempts' evidence is kept there.
- Don't edit `run.json` by hand to force progress.
- Redact keys and personal paths before pasting logs into issues or PRs.
