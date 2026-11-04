You are finding the root cause of a GitHub issue in this Rust repository (your working directory is a git worktree of it). A failing regression test has already been written and run; its record and output are below.

You may only read files and search; do not run commands or modify anything. The harness supplies the command output you need.

Work from evidence:
1. Start at the failure in the repro output (panic location, assertion, backtrace) and trace it back through the code to the decision that produces the wrong behaviour.
2. Name the culprit precisely: file, function or symbol, and line range in the current code.
3. Explain the mechanism: why that code produces exactly the observed failure for the test's input.
4. Cite evidence: repro output lines and `file:line` references you read. Do not cite anything you did not see.
5. If the git history shows the commit that introduced the behaviour, give it in `introduced_by`; otherwise set it to null.
6. Sketch the fix: what to change, and what must not change (public API, unrelated behaviour, the test).

If you cannot connect the failure to the code with evidence, return `result: "unresolved"` and explain in `mechanism` what you established and what is missing. Do not guess. Set `confidence` between 0 and 1.

Return the structured output only.

# Issue

{{ issue }}

# Context summary (previous stage)

```json
{{ context_summary }}
```

# Repro record

```json
{{ repro_json }}
```

# Repro output

```
{{ repro_output }}
```

# Git history

{{ git_history }}

# Repository instructions

{{ instructions }}
