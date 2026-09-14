#!/usr/bin/env bash
# A/B benchmark: fastbrew vs the Ruby Homebrew, both inside the same sandbox.
# Usage: scripts/bench.sh [DIR] [-- extra "cmd args" ...]
# DIR defaults to target/sandbox-ab. A sandboxed Homebrew clone is created on
# first use (scripts/sandbox.sh brew). Nothing touches the host's Homebrew.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sandbox="$repo_root/scripts/sandbox.sh"
dir="${1:-${FASTBREW_SANDBOX:-$repo_root/target/sandbox-ab}}"

cargo build --release --manifest-path "$repo_root/Cargo.toml" >/dev/null
fb="$repo_root/target/release/fastbrew"
[[ -d "$dir/prefix" ]] || "$sandbox" create "$dir" >/dev/null
[[ -x "$dir/prefix/bin/brew" ]] || "$sandbox" brew "$dir" >/dev/null
eval "$("$sandbox" env "$dir")"
brew="$dir/prefix/bin/brew"

# Warm both: brew installs portable ruby and builds its caches on first use.
"$brew" info jq >/dev/null 2>&1 || true
"$brew" search --desc json >/dev/null 2>&1 || true
"$fb" info jq >/dev/null 2>&1 || true

python3 - "$brew" "$fb" "$@" <<'PY'
import subprocess, sys, time, statistics
brew, fb = sys.argv[1], sys.argv[2]
cases = ["info jq", "search --desc json", "deps --tree jq", "list", "outdated",
         "uses --installed openssl@3", "leaves", "search ripgrep", "--version"]
extra = [a for a in sys.argv[3:] if a != "--"]
cases += extra

def run(exe, args, n):
    times = []
    for _ in range(n):
        t = time.perf_counter()
        subprocess.run([exe, *args], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        times.append(time.perf_counter() - t)
    return statistics.median(times) * 1000

print(f"{'command':<30} {'brew (ms)':>10} {'fastbrew (ms)':>14} {'speedup':>9}")
for c in cases:
    args = c.split()
    b = run(brew, args, 3)
    f = run(fb, args, 10)
    print(f"{c:<30} {b:>10.1f} {f:>14.1f} {b / f:>8.0f}x")
PY
