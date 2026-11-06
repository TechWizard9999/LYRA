{#- Prompt passages for the `python-pytest` execution profile (PLAN §13). The stage prompts
    import this file as `profile` and call its macros. -#}

{%- macro context_step() -%}
3. Decide which test directory a pytest regression test belongs in (from the pytest configuration and the existing `test_*.py` files), and the pytest command that runs the repository's tests.
{%- endmacro %}

{%- macro context_fields() -%}
- `crate`: always "" (pytest selects a test by its node id, not by a package name).
- `test_command`: e.g. `python3 -m pytest tests/`.
{%- endmacro %}

{%- macro repo_heading() -%}
Project layout (pytest configuration, test directories, interpreter)
{%- endmacro %}

{%- macro repro_context_hint() -%}
Candidate files, the test directory most likely involved, and the test command the repository uses:
{%- endmacro %}

{%- macro repro(issue_number) -%}
## Where the test goes

Write exactly one new file: a pytest test module named `test_issue_{{ issue_number }}.py` in the repository's existing test directory (the directory named by `testpaths` in the pytest configuration; otherwise `tests/`, or wherever the existing `test_*.py` files live). Never edit existing files: no production code, no `conftest.py`, no pytest configuration (`pytest.ini`, `pyproject.toml`, `setup.cfg`, `tox.ini`), no dependency files.

Also allowed: new non-Python fixture files under a `tests/` or `fixtures/` directory, and an empty `__init__.py` when the test directory must be a package. The harness rejects every other change, including new `conftest.py` files, helper `.py` modules, `*.pth` files and `sitecustomize.py`, so keep the diff to the test file.

## What the test must do

- One module-level test function named `test_issue_{{ issue_number }}_<what_it_checks>`, for example `test_issue_{{ issue_number }}_rejects_short_frame`. Import the code under test the way the existing tests do. No `skip`, `skipif` or `xfail` markers, no `parametrize`, no `pytest.raises` that would hide the bug, and no fixtures that the repository does not already define. The test must fail **because the bug exists** and pass once the bug is fixed.
- It must fail on an `assert` whose message describes the bug, for example `assert parse(frame) is None, "short frame must be rejected"`. An import or collection error, a missing dependency, a test that does not run, or an unrelated exception is NOT a reproduction.
- Keep it small and deterministic: no network, no timing dependence, no reliance on the current directory beyond the repository's own fixtures.
- Do not execute commands. Supply the pytest node id; the harness runs `python3 -m pytest <test_name>` with the repository's `addopts` neutralised, checks that exactly this one test executed and failed with your `expected` assertion text, and commits the validated reproduction. Only the harness can establish that reproduction succeeded.
{%- endmacro %}

{%- macro repro_fields() -%}
- `crate`: "" (empty: pytest needs no package name).
- `test_file`: repository-relative path with forward slashes, for example `tests/test_issue_7.py`.
- `test_name`: the pytest node id `<test_file>::<function>`, for example `tests/test_issue_7.py::test_issue_7_rejects_short_frame`.
- `command`: `python3 -m pytest <test_name>`.
{%- endmacro %}

{%- macro repro_registration_field() -%}
- `registration_file`: null (pytest discovers the test module by its file name).
{%- endmacro %}

{%- macro fix_lock(issue_number, test_file) -%}
- Do NOT modify `{{ test_file }}`, anything else in its test directory except other `test_*.py` modules (package `__init__.py` files, helper modules and data files are locked, and no new modules may be added there), any `conftest.py`, the pytest configuration (`pytest.ini`, `pytest.toml`, `[tool.pytest...]` in `pyproject.toml`, `[tool:pytest]` in `setup.cfg`, `[pytest]` in `tox.ini`), `sitecustomize.py`, `usercustomize.py` or `*.pth` files. Do not add `skip` or `xfail` markers or collection hooks anywhere. The harness compares these with their frozen form and rejects the fix if any of them changed.
{%- endmacro %}
