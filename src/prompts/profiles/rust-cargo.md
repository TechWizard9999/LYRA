{#- Prompt passages for the `rust-cargo` execution profile (PLAN §13). The stage prompts
    import this file as `profile` and call its macros; everything Cargo-specific in the
    prompts lives here. -#}

{%- macro context_step() -%}
3. Decide which workspace crate (package name from the metadata below) a regression test belongs in, and the `cargo test` command that would run tests for it.
{%- endmacro %}

{%- macro context_fields() -%}
- `crate`: the package name for the regression test, or "" if you cannot tell.
- `test_command`: e.g. `cargo test -p <crate>`.
{%- endmacro %}

{%- macro repo_heading() -%}
Workspace (cargo metadata)
{%- endmacro %}

{%- macro repro_context_hint() -%}
Candidate files, the crate most likely involved, and the test command the repository uses:
{%- endmacro %}

{%- macro repro(issue_number) -%}
## Where the test goes

Write exactly one new file. Never edit existing source files, except for the single registration line of option B.

- **Option A (preferred): an integration test** at `<crate dir>/tests/issue_{{ issue_number }}.rs`, using only the crate's public API. `<crate dir>` is the directory holding that crate's `Cargo.toml`; for a single-crate repository that is the root, so the file is `tests/issue_{{ issue_number }}.rs`.
- **Option B (only when the bug can be reached only through a private API): a unit-test module** in a new file `src/<module>/issue_{{ issue_number }}_tests.rs` next to the module it tests, registered by adding exactly this one line to the existing file that owns that module (`src/<module>/mod.rs`, `src/<module>.rs` or `src/lib.rs`):

  ```rust
  #[cfg(test)] mod issue_{{ issue_number }}_tests;
  ```

  Nothing else in that file may change. The new test file starts with `use super::*;`.

Also allowed: new fixture files under a `tests/` or `fixtures/` directory, and new entries under `[dev-dependencies]` in `Cargo.toml` (nothing else in `Cargo.toml`). The harness rejects every other change (edits to existing source, other `Cargo.toml` sections, `.cargo/`, reformatting of other files), so keep the diff to the test file.

## What the test must do

- One `#[test]` function named `issue_{{ issue_number }}_<what_it_checks>`, for example `issue_{{ issue_number }}_rejects_short_frame`. No `#[ignore]`, no `#[should_panic]` that would hide the bug. The test must fail **because the bug exists** and pass once the bug is fixed.
- It must fail on an assertion or panic whose message describes the bug, for example `assert_eq!(parse(&frame), Err(Error::TooShort), "short frame must be rejected")` or `panic!("expected an error for a 2-byte frame, got {out:?}")`. A compile error, a missing dependency, a test that does not run, or an unrelated panic is NOT a reproduction.
- Keep it small and deterministic: no network, no timing dependence, no reliance on the current directory beyond the repository's own fixtures.
- Do not execute commands. Supply `cargo test -p <crate> <test_name> -- --exact` for the harness to run. `<test_name>` must be the fully qualified Rust test name: for option A the bare function name; for option B the module path, for example `parser::issue_{{ issue_number }}_tests::issue_{{ issue_number }}_rejects_short_frame`.
- The harness runs that exact command, checks that the test executed and failed with your `expected` assertion text, formats the new file, and commits the validated reproduction. Only the harness can establish that reproduction succeeded.
{%- endmacro %}

{%- macro repro_fields() -%}
- `crate`: the cargo package name (the `name` in that crate's `Cargo.toml`, as accepted by `cargo test -p`).
- `test_file`: repository-relative path with forward slashes.
- `test_name`: exactly as cargo prints it (see above).
- `command`: `cargo test -p <crate> <test_name> -- --exact`.
{%- endmacro %}

{%- macro repro_registration_field() -%}
- `registration_file`: for option B, the source file that received the `#[cfg(test)] mod ...;` line; otherwise null.
{%- endmacro %}

{%- macro fix_lock(issue_number, test_file) -%}
- Do NOT modify `{{ test_file }}`, its `#[cfg(test)] mod issue_{{ issue_number }}_tests;` registration line, any `[[test]]`, `autotests` or `[dev-dependencies]` entry in `Cargo.toml`, or anything under `.cargo/`. Do not add `#[ignore]` or `cfg` gates anywhere. The harness compares these with their frozen form and rejects the fix if any of them changed.
{%- endmacro %}
