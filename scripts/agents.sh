#!/usr/bin/env sh
# Manage the agent context in .agents/.
#   sync           regenerate .claude/skills from .agents/skills (Claude Code only reads .claude/skills)
#   check          fail if a skill is malformed or .claude/skills drifted from .agents/skills (working tree)
#   check-staged   the same check on the staged blobs (what a commit would contain); used by the pre-commit hook
#   install-hooks  point this clone's git hooks at scripts/git-hooks
set -eu
cd "$(dirname "$0")/.."

src=".agents/skills"
dst=".claude/skills"
marker=".generated"

validate() {
  status=0
  for dir in "$src"/*/; do
    name="$(basename "$dir")"
    file="$dir/SKILL.md"
    if [ ! -f "$file" ]; then echo "!! $name: missing SKILL.md"; status=1; continue; fi
    # Frontmatter = lines 2..(first "---" after line 1). It must open on line 1 and be closed.
    # CR is stripped so CRLF checkouts (Windows) parse the same as LF.
    fm="$(awk '{sub(/\r$/,"")} NR==1{if($0!="---")exit 1;next} /^---$/{closed=1;exit} {print} END{if(!closed)exit 1}' "$file")" \
      || { echo "!! $name: SKILL.md must start with --- frontmatter closed by a second ---"; status=1; continue; }
    fm_name="$(printf '%s\n' "$fm" | sed -n 's/^name:[[:space:]]*//p' | head -n 1)"
    if [ "$fm_name" != "$name" ]; then echo "!! $name: frontmatter name is '$fm_name', expected '$name'"; status=1; fi
    if ! printf '%s\n' "$fm" | grep -qE '^description:[[:space:]]*[^[:space:]]'; then
      echo "!! $name: frontmatter needs a non-empty description"; status=1
    fi
  done
  return $status
}

sync() {
  validate || exit 1
  rm -rf "$dst"
  mkdir -p "$dst"
  cp -R "$src"/. "$dst"/
  printf 'Generated from .agents/skills by scripts/agents.sh sync. Do not edit here.\n' > "$dst/$marker"
  echo "synced $(ls "$src" | wc -l | tr -d ' ') skills to $dst"
}

check() {
  validate || exit 1
  if [ ! -d "$dst" ]; then echo "!! $dst missing: run ./scripts/agents.sh sync"; exit 1; fi
  if ! diff -r -x "$marker" "$src" "$dst" >/dev/null 2>&1; then
    echo "!! $dst is out of date with $src: run ./scripts/agents.sh sync and commit the result"
    diff -rq -x "$marker" "$src" "$dst" || true
    exit 1
  fi
  echo "ok: $dst matches $src"
}

# The commit contains the index, not the working tree: a contributor can edit both copies and stage only one.
# Export the staged tree and compare the staged .agents/skills with the staged .claude/skills.
check_staged() {
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  tree="$(git write-tree)" || { echo "!! cannot read the index (unresolved merge?)"; exit 1; }
  for path in "$src" "$dst"; do
    git archive "$tree" "$path" 2>/dev/null | tar -x -C "$tmp" 2>/dev/null || true
  done
  if [ ! -d "$tmp/$src" ]; then echo "!! $src is missing from the staged tree"; exit 1; fi
  ( src="$tmp/$src"; validate ) || exit 1
  if [ ! -d "$tmp/$dst" ] || ! diff -r -x "$marker" "$tmp/$src" "$tmp/$dst" >/dev/null 2>&1; then
    echo "!! the staged $dst differs from the staged $src, so this commit would carry stale skill copies."
    echo "   run ./scripts/agents.sh sync and stage both directories (git add .agents/skills .claude/skills)"
    diff -rq -x "$marker" "$tmp/$src" "$tmp/$dst" 2>/dev/null | sed "s#$tmp/##g" || true
    exit 1
  fi
  echo "ok: staged $dst matches staged $src"
}

install_hooks() {
  git config core.hooksPath scripts/git-hooks
  chmod +x scripts/git-hooks/* 2>/dev/null || true
  echo "git hooks now read from scripts/git-hooks (commit-msg, pre-commit)"
}

case "${1:-}" in
  sync) sync ;;
  check) check ;;
  check-staged) check_staged ;;
  install-hooks) install_hooks ;;
  *) echo "usage: scripts/agents.sh sync|check|check-staged|install-hooks"; exit 2 ;;
esac
