---
name: acceptance-run
description: Run LYRA end to end against a seeded-bug scratch crate with a real agent runtime to prove the pipeline works (intake to draft PR, or a clean stop). Use before releases and after changes to stages, guards, adapters, or the engine.
---

# Acceptance run

Manual, uses a real agent runtime and `gh`. Not part of CI. PLAN §9 step 2 and 5 describe the target: a draft PR whose head is the verified commit, with a test that fails before the fix and passes after; plus one unreproducible issue that stops cleanly.

## Prerequisites
`lyra doctor` is green for your chosen runtime and `gh auth status` is authenticated (`onboard` skill). Use your own account and a **throwaway GitHub repo you own**; never point this at someone else's repository.

## Setup
1. Create a small Rust crate (a library with one function and a unit test) in a new GitHub repo you own; clone it locally.
2. Seed a bug: e.g. an off-by-one in a parsing function with documented behaviour. Commit and push to `main`.
3. Open a GitHub issue describing the bug the way a user would (symptoms, input, expected vs actual). Don't name the file or the fix.
4. Use an isolated home so you don't mix with real runs:
```sh
export LYRA_HOME=$(mktemp -d)        # PowerShell: $env:LYRA_HOME = New-Item -ItemType Directory -Path (Join-Path $env:TEMP "lyra-accept") -Force | % FullName
cargo build --release --locked
./target/release/lyra doctor         # creates config under LYRA_HOME; edit runtime/auth if needed
```

## Run
- Dashboard: `./target/release/lyra serve`, then enter the repo path and issue URL, pick the runtime, Run. Watch the stage strip, live log, artifacts.
- Or CLI: `./target/release/lyra run --repo <path> --issue <url>` (stop `serve` first; they share a writer lock).

## Pass criteria
- S2: `20-repro/` has a test in its own file that fails on the recorded assertion; the repro commit exists.
- S3: `30-rca/rca.json` is `supported` and names the seeded culprit.
- S4: `40-fix/fix.diff` touches production code only; the test file is unchanged.
- S5: `50-verify/result.json` shows build, repro (`1 passed`), tests, fmt all green; `run.json.verified_commit` set.
- S6: a **draft** PR exists whose head SHA equals `verified_commit`; body has the disclosure line; commits carry your identity and no AI co-author trailer.
- Nothing was merged or marked ready.

## Negative runs (each should stop with evidence and a Retry button)
- An issue that cannot be reproduced → `stopped: not_reproduced`.
- Cancel mid-run → status `cancelled`, process tree gone, Retry available.
- Kill `lyra serve` mid-run, restart → run marked `interrupted`; retry from that stage works.
- Retry from `fix` after a deliberately failing verify → previous outputs under `attempts/`.

## Cleanup
Close the draft PR and delete the branch on your throwaway repo. Remove `$LYRA_HOME`. Record in your PR which runtime and CLI versions you tested.
