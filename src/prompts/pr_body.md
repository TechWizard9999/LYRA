## Issue

Fixes {% if issue_ref %}{{ issue_ref }}{% else %}#{{ issue_number }}{% endif %}{% if issue_title %}: {{ issue_title }}{% endif %}
{%- if issue_url %}
({{ issue_url }}){% endif %}

## Reproduction

{% if repro -%}
A regression test was written before the fix and failed on the recorded assertion.

- Test: `{{ repro.test_name }}` in `{{ repro.test_file }}`{% if repro.crate %} (crate `{{ repro.crate }}`){% endif %}
- Run it: `{{ repro.command }}`
- Failed before the fix with: `{{ repro.expected }}`
{%- if repro.notes %}

{{ repro.notes }}
{%- endif %}
{%- else -%}
_Reproduction record not recorded._
{%- endif %}

## Root cause

{% if rca -%}
{{ rca.mechanism }}
{% if rca.culprits %}
Culprits:
{% for c in rca.culprits %}
- `{{ c.file }}`{% if c.symbol %} `{{ c.symbol }}`{% endif %}{% if c.lines and c.lines | length == 2 %} (lines {{ c.lines[0] }}-{{ c.lines[1] }}){% endif %}
{%- endfor %}
{% endif %}
{%- if rca.evidence %}
Evidence:
{% for e in rca.evidence %}
- {{ e }}
{%- endfor %}
{% endif %}
{%- if rca.introduced_by and rca.introduced_by.commit %}
Introduced by `{{ rca.introduced_by.commit }}`.
{% endif %}
{%- else -%}
_Root cause analysis not recorded._
{% endif %}
## Change

{% if scope -%}
{{ scope.files | length }} file(s), +{{ scope.total_added }} / -{{ scope.total_removed }} lines.

| File | Added | Removed |
|---|---|---|
{% for f in scope.files -%}
| `{{ f.path }}` | +{{ f.added }} | -{{ f.removed }} |
{% endfor %}
{%- if scope.test_files %}
Other test files touched: {% for t in scope.test_files %}`{{ t }}`{% if not loop.last %}, {% endif %}{% endfor %}
{% endif %}
{%- if scope.warnings %}
Warnings:
{% for w in scope.warnings %}
- {{ w }}
{%- endfor %}
{% endif %}
{%- else -%}
_Change scope not recorded._
{% endif %}
## Verification

{% if verify -%}
{% if verify.ok %}All checks passed{% else %}Some checks failed{% endif %} on `{{ verified_commit }}` (branch `{{ branch }}`).
{% if verify.repository_overrides %}
Repository verification overrides from `lyra.toml` were used. The table lists the effective commands.
{% endif %}

| Step | Command | Result | Time |
|---|---|---|---|
{% for s in verify.steps -%}
| {{ s.name }} | `{{ s.command }}` | {% if s.ok %}pass{% else %}fail{% endif %} | {{ (s.duration_ms / 1000) | round(1) }} s |
{% endfor %}
{%- else -%}
Verification results not recorded. Verified commit: `{{ verified_commit }}` (branch `{{ branch }}`).
{% endif %}
## Disclosure

{{ disclosure }}
