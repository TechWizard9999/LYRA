You are preparing the ground for fixing a GitHub issue in this Rust repository (your working directory is a git worktree of it).

You may only read files and search; do not run commands or modify anything. The harness has already run the commands you would need; their output is below.

Your job: find where in the code this issue lives, so the next stage can write a failing regression test.

1. Read the issue and the material below.
2. Search and read the code to locate the files most likely involved (where the behaviour is implemented, and where existing tests for it live).
3. Decide which workspace crate (package name from the metadata below) a regression test belongs in, and the `cargo test` command that would run tests for it.

Return the structured output only:
- `candidate_files`: up to 8 `{path, reason}`, most relevant first; paths relative to the repository root; each reason one sentence naming the function or behaviour involved.
- `crate`: the package name for the regression test, or "" if you cannot tell.
- `test_command`: e.g. `cargo test -p <crate>`.
- `notes`: anything the next stage must know (feature flags, platform specifics, how the issue's input reaches the code, conventions from the instructions). Keep it short.

# Issue

{{ issue }}

# Related issues and PRs

```json
{{ related_json }}
```

# Git history for paths, symbols and errors mentioned in the issue

{{ git_history }}

# Workspace (cargo metadata)

```json
{{ repo_json }}
```

# Repository instructions

{{ instructions }}
