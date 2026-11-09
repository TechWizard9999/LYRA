#!/usr/bin/env sh
# LYRA setup for Linux / macOS. Non-destructive and safe to run again:
# checks tools, builds the release binary, writes ~/.lyra/config.toml only if it is missing,
# and runs `lyra doctor`.
set -u
cd "$(dirname "$0")" || exit 1

problems=0
ok() { printf 'ok  %s\n' "$1"; }
bad() { printf '!!  %s\n' "$1"; problems=$((problems + 1)); }
have() { command -v "$1" >/dev/null 2>&1; }

echo "== Checking tools"
if have cargo; then ok "cargo: $(cargo --version)"; else bad "cargo not found: install Rust from https://rustup.rs"; fi
if have git; then ok "git: $(git --version)"; else bad "git not found"; fi
if have gh; then
  if gh auth status >/dev/null 2>&1; then ok "gh: authenticated"; else bad "gh is installed but not authenticated: run 'gh auth login'"; fi
else
  bad "gh (GitHub CLI) not found: install from https://cli.github.com and run 'gh auth login'"
fi
runtimes=""
have claude && runtimes="$runtimes claude"
have codex && runtimes="$runtimes codex"
if [ -n "$runtimes" ]; then ok "runtime(s) on PATH:$runtimes"
else bad "neither claude nor codex on PATH: 'npm i -g @anthropic-ai/claude-code' and/or 'npm i -g @openai/codex'"; fi

if ! have cargo; then
  echo; echo "Cannot build without cargo. Fix the items above and run ./setup.sh again."; exit 1
fi

echo; echo "== Building (cargo build --release --locked)"
if ! cargo build --release --locked; then
  echo; echo "Build failed; see SETUP.md (Troubleshooting)."; exit 1
fi

case "${LYRA_HOME:-}" in
  *[![:space:]]*) lyra_home="$LYRA_HOME" ;;
  *) lyra_home="$HOME/.lyra" ;;
esac
config="$lyra_home/config.toml"

echo; echo "== lyra doctor"
lyra="$(pwd)/target/release/lyra"
doctor_ok=1
# The binary owns the default config and creates it only when missing.
"$lyra" doctor || doctor_ok=0
# Single-quote a shell word, including any embedded apostrophes.
lyra_command="'$(printf '%s' "$lyra" | sed "s/'/'\\\\''/g")'"

echo; echo "== Next steps"
if [ "$problems" -gt 0 ] || [ "$doctor_ok" -eq 0 ]; then
  echo "Fix the items marked !! above (details in SETUP.md), then run ./setup.sh again."
fi
echo "  1. Edit $config (runtime, auth, model) if needed."
printf '  2. Start the dashboard:  %s serve\n' "$lyra_command"
echo "  3. In the browser, enter the path of an already-cloned local repository with a supported profile and a GitHub issue, then press Run."
printf '     Or from a terminal:   %s run --repo /path/to/repo --issue https://github.com/owner/repo/issues/123\n' "$lyra_command"
