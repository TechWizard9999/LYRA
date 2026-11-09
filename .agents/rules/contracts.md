# Contract rules

Contract modules are shared by every stage, both adapters, the CLI, and the dashboard:

`error`, `events`, `state`, `config`, `artifacts`, `proc`, `git`, `providers/mod.rs`, `stages/mod.rs`, `engine`.

A change to one needs a **"Contract change"** note in the PR description (what changed, who is affected, compatibility). Use the `change-contract` skill.

## On-disk compatibility (the part that bites)

`run.json` and `events.jsonl` from earlier versions must stay readable; users retry old runs.

- New struct fields: `#[serde(default)]` (and `skip_serializing_if` for optionals). Never make an existing field required.
- Enum variants: add only; never rename or remove a serialized variant. Serialized names are `snake_case` (`rename_all`).
- `StageId`, `ErrorKind`, `StopReason`, `Status`, `EventKind`, `AgentEvent` are all serialized somewhere. Adding a variant means the dashboard (`web/views.rs`, `static/app.js`) must handle it.
- `events.jsonl` is append-only; replay tolerates a truncated last line. Don't change that.
- Old runs without `frozen_repro` must retry from Repro; missing issue identity requires retry from Intake (PLAN §12). Don't silently invent values for missing fields.

## Artifacts

- Layout is PLAN §7: `artifacts/<NN-stage>/…` (`00-issue.md`, `10-context/`, `20-repro/`, `30-rca/`, `40-fix/`, `50-verify/`, `60-pr/`, `transcripts/`). New artifacts go under their stage's dir and are announced with an `artifact_written` event.
- Writes go through `RunDir`. Superseded outputs move to `attempts/<n>/` via `archive_from`.

## Provider trait and `StageCtx`

- `Agent` (`providers/mod.rs`) and `AgentTask`/`AgentOutput` are implemented by both adapters and constructed by every stage; adding a required field breaks both. Prefer a defaulted field.
- `StageCtx` and `Stage` (`stages/mod.rs`) likewise.

## Config

- New config keys have defaults so an existing `~/.lyra/config.toml` still loads. Update the default text written by `Config::ensure_default`, README's Configuration section, and PLAN §7.
- Per-repo `lyra.toml` may override verify commands only (`[verify]` for `rust-cargo`, `[profiles.<id>.verify]` for any profile) plus the profiles' own production-code declarations (`packages` for `python-pytest`, `sources` for `js-vitest`). Don't widen that.
- The `ExecutionProfile` trait (`src/profile/mod.rs`) is shared by every stage: a new method needs a default so the other profiles keep compiling, and a PR note.

## Docs to update with a contract change

PLAN.md (the relevant section), README.md / SETUP.md if user-visible, and this file if a rule here changes.
