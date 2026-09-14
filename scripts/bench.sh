#!/usr/bin/env bash
# A/B benchmark: fastbrew vs the Ruby Homebrew, both inside the sandbox.
# Usage: scripts/bench.sh [DIR]   (DIR as in scripts/sandbox.sh)
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sandbox="$repo_root/scripts/sandbox.sh"
dir="${1:-${FASTBREW_SANDBOX:-$repo_root/target/sandbox}}"

cargo build --release --manifest-path "$repo_root/Cargo.toml" >/dev/null
fb="$repo_root/target/release/fastbrew"
[[ -d "$dir/prefix" ]] || "$sandbox" create "$dir" >/dev/null
[[ -x "$dir/prefix/bin/brew" ]] || "$sandbox" brew "$dir" >/dev/null
eval "$("$sandbox" env "$dir")"
brew="$dir/prefix/bin/brew"

# Warm both: brew installs portable ruby on first run.
"$brew" --version >/dev/null 2>&1 || true
"$fb" update --quiet >/dev/null 2>&1 || true

run() {
  local label="$1"; shift
  if command -v hyperfine >/dev/null; then
    hyperfine -N --warmup 2 --runs 10 -n "brew $label" "$brew $*" -n "fastbrew $label" "$fb $*" 2>&1 | grep -E 'Time|brew|fastbrew|±' | sed 's/^/  /'
  else
    printf '%-28s' "brew $label"; { /usr/bin/time -p "$brew" "$@" >/dev/null 2>/dev/null; } 2>&1 | awk '/real/{print $2 "s"}'
    printf '%-28s' "fastbrew $label"; { /usr/bin/time -p "$fb" "$@" >/dev/null 2>/dev/null; } 2>&1 | awk '/real/{print $2 "s"}'
  fi
}

run "info jq" info jq
run "search --desc json" search --desc json
run "deps --tree jq" deps --tree jq
run "list" list
run "outdated" outdated
run "uses --installed openssl@3" uses --installed openssl@3
