# .agents/

Canonical contributor rules and playbooks live here alongside the root `AGENTS.md` entry point. Tool-specific entry points and generated copies refer to those sources.

```
AGENTS.md              canonical entry: purpose, architecture map, rules, definition of done
CLAUDE.md              Claude Code entry; imports AGENTS.md, points here
.agents/
  README.md            this file
  rules/               what must always be true (read the ones that match your change)
  skills/<name>/       task playbooks, each a SKILL.md with name + description frontmatter
  decisions/           short design-decision log (ADR style)
scripts/agents.sh|ps1  sync, check, install-hooks
.claude/skills/        GENERATED copy of .agents/skills for Claude Code. Never edit by hand.
```

## Why a generated `.claude/skills/`

Claude Code only discovers skills under `.claude/skills/`. Codex reads `.agents/skills/` directly. We keep one source (`.agents/skills/`) and copy it, instead of symlinking, because symlinks are unreliable on Windows.

```sh
./scripts/agents.sh sync            # regenerate .claude/skills (after editing a skill)
./scripts/agents.sh check           # fail if .claude/skills drifted (CI runs this)
./scripts/agents.sh check-staged    # the same check on the staged blobs (the pre-commit hook runs this)
./scripts/agents.sh install-hooks   # commit-msg + pre-commit hooks for this clone
```

PowerShell (three separate commands; `|` would be a pipeline there):

```powershell
.\scripts\agents.ps1 sync
.\scripts\agents.ps1 check
.\scripts\agents.ps1 install-hooks
```

`check-staged` exists only in the shell script: the pre-commit hook runs through `sh`, including under Git for Windows.

## Rules (`rules/`)

| File | Read it when |
|---|---|
| `development.md` | writing any Rust: layout, errors, style, dependencies |
| `testing.md` | adding or changing tests, fixtures, the JS tests |
| `contracts.md` | touching a contract module or on-disk formats |
| `security.md` | touching guards, providers, proc, the web server, prompts |
| `git-and-prs.md` | committing, pushing, opening a PR |
| `windows.md` | working on Windows |

## Skills (`skills/`)

| Group | Skills |
|---|---|
| Development | `change-a-stage` `add-guard` `add-provider-adapter` `add-prompt-or-schema` `add-artifact` `change-contract` `add-dashboard-feature` |
| Testing | `write-tests` `record-fixture` `pre-pr-check` `acceptance-run` |
| Onboarding | `onboard` `debug-a-run` |
| Process | `commit-and-pr` `review-checklist` `update-docs` |

## Adding or changing a skill

1. Create `.agents/skills/<kebab-name>/SKILL.md` with frontmatter `name:` (same as the folder) and `description:` (one or two sentences saying what it does and when to use it; this is what an agent matches on).
2. Keep it a checklist an agent can follow: what to read first, steps, how to verify. Link to rules and PLAN sections instead of copying them.
3. Run `./scripts/agents.sh sync` and commit both `.agents/skills/` and `.claude/skills/`.
4. List it in the table above.

## Adding a rule or decision

- Rules go in `rules/` (small, specific, verifiable). Put repository-wide rules in the canonical `AGENTS.md`; keep tool-specific entry points as pointers to it.
- Design choices that someone might re-argue go in `decisions/NNNN-title.md`.

## Not in this folder

`.claude/settings.json` (shared permission allowlist) is a per-team decision; add it separately if wanted. Prompts for LYRA's own agent calls are product code and stay in `src/prompts/`.
