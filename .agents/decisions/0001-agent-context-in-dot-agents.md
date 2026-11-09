# 0001: Agent context lives in `.agents/`

**Context.** The team uses Claude Code and Codex. Codex and most agents read `AGENTS.md`; Claude Code reads only `CLAUDE.md` and discovers skills only under `.claude/skills/`. We want one source of truth, one folder, no per-directory instruction files, and a setup that works on Windows.

**Decision.**
- Canonical content lives in `.agents/` (`rules/`, `skills/`, `decisions/`). `AGENTS.md` is the thin canonical entry; `CLAUDE.md` imports it.
- `.claude/skills/` is a generated copy of `.agents/skills/`, produced by `scripts/agents.sh|ps1 sync` and verified by `check` in CI and the pre-commit hook. Copy, not symlink, because symlinks are unreliable on Windows.
- No `CLAUDE.md` or `AGENTS.md` in subdirectories.
- Other agents (Gemini, Cursor, …) are out of scope until the team uses them; adding one means a new pointer file, not new content.

**Consequences.** Editing a skill is a two-step (edit, `sync`), enforced by `check`. Generated files are committed so Claude works on a fresh clone without running anything. A shared `.claude/settings.json` is deliberately not part of this.
