#!/usr/bin/env sh
# Regression tests for `scripts/agents.sh check`: mutate a skill in a scratch copy and assert the
# checker accepts or rejects it. Run from anywhere; touches nothing in the working tree.
set -u
root="$(cd "$(dirname "$0")/.." && pwd)"
failures=0

new_copy() {
  tmp="$(mktemp -d)"
  mkdir -p "$tmp/scripts" "$tmp/.claude"
  cp -R "$root/.agents" "$tmp/.agents"
  cp -R "$root/.claude/skills" "$tmp/.claude/skills"
  cp "$root/scripts/agents.sh" "$tmp/scripts/agents.sh"
}

# expect <pass|fail> <description> <mutation: sh snippet run in the copy, applied to both trees>
expect() {
  want="$1"; label="$2"; mutate="$3"
  new_copy
  ( cd "$tmp" && for tree in .agents/skills .claude/skills; do
      f="$tree/add-guard/SKILL.md"; eval "$mutate"
    done )
  if ( cd "$tmp" && sh scripts/agents.sh check >/dev/null 2>&1 ); then got=pass; else got=fail; fi
  if [ "$got" = "$want" ]; then echo "ok   $label"; else echo "FAIL $label (expected $want, got $got)"; failures=$((failures + 1)); fi
  rm -rf "$tmp"
}

expect pass "untouched tree"                      ':'
expect fail "missing closing ---"                 'awk "BEGIN{n=0} /^---\$/{n++; if(n==2) next} {print}" "$f" > "$f.new" && mv "$f.new" "$f"'
expect fail "no opening ---"                      'sed 1d "$f" > "$f.new" && mv "$f.new" "$f"'
expect fail "wrong name"                          'sed "s/^name: add-guard/name: other/" "$f" > "$f.new" && mv "$f.new" "$f"'
expect fail "empty description"                   'sed "s/^description:.*/description:/" "$f" > "$f.new" && mv "$f.new" "$f"'
expect fail "missing description"                 'sed "/^description:/d" "$f" > "$f.new" && mv "$f.new" "$f"'
expect pass "CRLF line endings (Windows checkout)" 'sed "s/\$/\\r/" "$f" > "$f.new" && mv "$f.new" "$f"'
expect pass "description starting with t or backslash" 'sed "s/^description:.*/description: terse \\\\ text/" "$f" > "$f.new" && mv "$f.new" "$f"'

# drift between the two trees must still fail
new_copy
echo "extra" >> "$tmp/.claude/skills/add-guard/SKILL.md"
if ( cd "$tmp" && sh scripts/agents.sh check >/dev/null 2>&1 ); then echo "FAIL drift not detected"; failures=$((failures + 1)); else echo "ok   drift between trees"; fi
rm -rf "$tmp"

[ "$failures" -eq 0 ] && echo "all checker tests passed" || { echo "$failures checker test(s) failed"; exit 1; }
