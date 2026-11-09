---
name: change-contract
description: Checklist for changing a shared contract module (error, events, state, config, artifacts, proc, git, providers/mod.rs, stages/mod.rs, engine) or any serialized on-disk format. Use before editing those files and when writing the PR note.
---

# Change a contract

Contract modules: `error`, `events`, `state`, `config`, `artifacts`, `proc`, `git`, `providers/mod.rs`, `stages/mod.rs`, `engine`. Details in `.agents/rules/contracts.md`.

## Before editing
1. Ask whether a change is needed or whether a stage-local, `guards.rs` or profile-local change would do. (`ExecutionProfile` in `src/profile/mod.rs` is a contract too: a trait change reaches every profile and the profile record saved in `run.json`.) Prefer the smaller blast radius.
2. Find every user: `rg` for the type/function across `src/`, including `web/` (`views.rs`, `api.rs`, `static/app.js`) and `main.rs`.
3. Check what is serialized: `RunState`/`RunRequest` (`run.json`), `Event`/`EventKind`/`AgentEvent` (`events.jsonl`, SSE, dashboard JS), `StageError`/`StopReason`/`StageOutcome`, config TOML, artifact JSON.

## Compatibility checklist
- [ ] Old `run.json` and `events.jsonl` still deserialize (new fields `#[serde(default)]`; no removed/renamed fields or variants; `snake_case` names unchanged).
- [ ] Old `~/.lyra/config.toml` still loads; defaults cover new keys; `Config::ensure_default` text updated.
- [ ] Both provider adapters and every stage still compile and behave; trait additions have defaults.
- [ ] Dashboard handles any new variant/status/event kind (server render and `app.js`), and `app.test.js` covers it.
- [ ] Retry-from-stage and interrupted-run handling unaffected (`engine`, `RunDir::archive_from`).
- [ ] Event replay tolerates truncated trailing lines (unchanged).
- [ ] Writer-lock and one-active-run semantics unchanged.

## Tests
- A round-trip test with the **old** shape (a JSON literal copied from before the change) deserializing into the new type.
- A round-trip test with the new shape.
- Behavioural tests for each stage/adapter/handler that uses the changed item.

## PR note (required)
Add a **Contract change** section to the PR description:
```
Contract change
- Module(s): <list>
- What changed: <one or two lines>
- Affects: <stages / adapters / CLI / dashboard / on-disk files>
- Compatibility: <old files still readable? migration? retry-from-X required for old runs?>
```
Update `docs/PLAN.md` (state/events in §5, storage in §7, §12 if compatibility rules change) and README/SETUP if user-visible (`update-docs`).
