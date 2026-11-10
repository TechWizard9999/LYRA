{#- Prompt passages for the `js-vitest` execution profile (PLAN §13). The stage prompts
    import this file as `profile` and call its macros. -#}

{%- macro context_step() -%}
3. Decide where a Vitest regression test belongs (next to the existing `*.test.*` / `*.spec.*` files, or in the repository's test directory), how the existing tests import the code under test, and the command that runs the repository's tests.
{%- endmacro %}

{%- macro context_fields() -%}
- `crate`: always "" (Vitest selects a test by its file and full name, not by a package name).
- `test_command`: e.g. `npx vitest run`.
{%- endmacro %}

{%- macro repo_heading() -%}
Project layout (package.json, Vitest configuration, test files, Node version)
{%- endmacro %}

{%- macro repro_context_hint() -%}
Candidate files, the existing test files most likely involved, and the test command the repository uses:
{%- endmacro %}

{%- macro repro(issue_number) -%}
## Where the test goes

Write exactly one new file: a Vitest test module named `issue-{{ issue_number }}.test.<ext>` (use `.ts` in a TypeScript project, otherwise the extension the existing tests use) in the directory where the existing tests live (a `tests/`, `test/` or `__tests__/` directory, or next to the code under test if that is the repository's convention). Never edit existing files: no production code, no setup files, no Vitest or Vite configuration, no `package.json`, no lockfiles, no `tsconfig.json`.

Also allowed: new non-code fixture files (JSON, text) in a test or fixture directory. The harness rejects every other change, including setup files, helper modules, snapshot files, mocks and anything under `node_modules`, so keep the diff to the test file.

## What the test must do

- One `describe` block named `issue {{ issue_number }}` containing one `it` (or `test`) whose name says what it checks, for example `describe('issue {{ issue_number }}', () => { it('rejects a short frame', ...) })`. Import the code under test the way the existing tests do, with `import { describe, it, expect } from 'vitest'`. No `.skip`, `.todo`, `.only`, `.fails`, `.each`, `vi.mock` of the code under test, or snapshot assertions. The test must fail **because the bug exists** and pass once the bug is fixed.
- Call the project's code in-process. Do not start child processes (`child_process`, `execa`): the harness rejects a regression it cannot attribute.
- It must fail on an assertion whose message describes the bug, for example `expect(parse(frame), 'short frame must be rejected').toBeNull()`. A syntax or import error, a missing dependency, a test that does not run, or an unrelated exception is NOT a reproduction.
- Keep it small and deterministic: no network, no timers that depend on real time, no reliance on the current directory beyond the repository's own fixtures.
- Do not execute commands. Supply the test file and the test's full name; the harness runs `npx vitest run <test_file> -t "<full name>"` through its own recorder, checks that exactly this one test executed and failed with your `expected` assertion text, and commits the validated reproduction. Only the harness can establish that reproduction succeeded.
{%- endmacro %}

{%- macro repro_fields() -%}
- `crate`: "" (empty: Vitest needs no package name).
- `test_file`: repository-relative path with forward slashes, for example `tests/issue-7.test.ts`.
- `test_name`: the test's full name, its `describe` names and the test name joined by ` > `, for example `issue 7 > rejects a short frame`. Escape a literal `>` as `\>` and a literal backslash as `\\` within a title segment. Use JSON escapes for the backslashes in the response.
- `command`: `npx vitest run <test_file> -t "<full name>"`.
{%- endmacro %}

{%- macro repro_registration_field() -%}
- `registration_file`: null (Vitest discovers the test module by its file name).
{%- endmacro %}

{%- macro fix_lock(issue_number, test_file) -%}
- Change only the project's production code: source files (`.js`, `.mjs`, `.cjs`, `.jsx`, `.ts`, `.mts`, `.cts`, `.tsx`) inside the source directories the repository declares (`files`, `main`, `exports` in `package.json`, `include` or `rootDir` in `tsconfig.json`, or `sources` under `[profiles.js-vitest]` in its `lyra.toml`). Other files there, such as JSON or text data, are frozen.
- Everything else is frozen and the harness rejects the fix if any of it is added, changed, deleted or renamed: `{{ test_file }}` and every other test module, setup files, mocks, snapshots (`__snapshots__`, `*.snap`), fixtures and test directories wherever they live, every file the failing test loaded that is not production code, every `package.json`, lockfiles, `.npmrc`, `tsconfig*.json`, Babel or SWC settings, every Vitest or Vite configuration, docs, CI files and `lyra.toml`. Existing tests cannot be updated, even if they encoded the buggy behaviour; say so in `notes` if one will now fail.
- LYRA installs nothing, so fix the bug without new or changed dependencies, and never add anything under `node_modules`. Do not add `.skip`, `.todo` or `.only` anywhere.
{%- endmacro %}
