#!/usr/bin/env bash
# A/B the installed state fastbrew and the Ruby Homebrew produce.
#
# Usage:
#   scripts/compat-check.sh [-a FASTBREW_SANDBOX] [-b BREW_SANDBOX] [-k] FORMULA...
#
#   -a DIR  sandbox for fastbrew      (default <repo>/target/sandbox-compat)
#   -b DIR  sandbox holding a Ruby brew, created by `scripts/sandbox.sh brew DIR`
#           (default $FASTBREW_AB_SANDBOX or <repo>/target/sandbox-ab)
#   -k      keep the sandboxes' installed state instead of resetting it first
#
# Installs FORMULA... with both tools in their own sandbox, then diffs:
#   * `list --versions`
#   * every symlink under bin, sbin, etc, include, lib, share, Frameworks, opt
#     and var/homebrew/linked (path -> target)
#   * every INSTALL_RECEIPT.json, with the volatile fields normalized
#   * the stdout of `info`, `deps --tree`, `list --versions`, `leaves` and
#     `uses --installed`
#
# Both prefixes are rewritten to @@PREFIX@@ and byte counts to @@SIZE@@, so only
# real differences survive. Exit status is 0 when everything matches, 1 when a
# diff is reported. Nothing outside the two sandboxes is touched: the host
# /opt/homebrew is never executed or written (see CLAUDE.md).
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fb_dir="$repo_root/target/sandbox-compat"
ab_dir="${FASTBREW_AB_SANDBOX:-$repo_root/target/sandbox-ab}"
keep=""

while getopts "a:b:kh" opt; do
  case "$opt" in
    a) fb_dir="$OPTARG" ;;
    b) ab_dir="$OPTARG" ;;
    k) keep=1 ;;
    h|*) sed -n '2,25p' "${BASH_SOURCE[0]}"; exit 1 ;;
  esac
done
shift $((OPTIND - 1))
[[ $# -gt 0 ]] || { echo "compat-check.sh: name at least one formula" >&2; exit 2; }
formulae=("$@")

forbidden() {
  case "$1" in
    /opt/homebrew|/opt/homebrew/*|/usr/local|/usr/local/*|"$HOME/Library/Caches/Homebrew"*)
      echo "compat-check.sh: refusing to use $1 (host Homebrew location)" >&2; exit 2 ;;
  esac
}
forbidden "$fb_dir"
forbidden "$ab_dir"

fastbrew_bin="$repo_root/target/release/fastbrew"
[[ -x "$fastbrew_bin" ]] || { echo "compat-check.sh: build it first: cargo build --release" >&2; exit 2; }
brew_bin="$ab_dir/prefix/bin/brew"
[[ -x "$brew_bin" ]] || {
  echo "compat-check.sh: no Ruby brew at $brew_bin; run: scripts/sandbox.sh brew $ab_dir" >&2
  exit 2
}

out_dir="$(mktemp -d "${TMPDIR:-/tmp}/compat-check.XXXXXX")"
trap 'rm -rf "$out_dir"' EXIT

# --------------------------------------------------------------------------
# Running each tool inside its own sandbox environment.
# --------------------------------------------------------------------------
env_file() { "$repo_root/scripts/sandbox.sh" env "$1"; }

run_fb() { ( set -a; eval "$(env_file "$fb_dir")"; set +a
             export HOMEBREW_NO_INSTALL_CLEANUP=1 HOMEBREW_NO_COLOR=1
             "$fastbrew_bin" "$@" ); }

run_ab() { ( set -a; eval "$(env_file "$ab_dir")"; set +a
             unset FASTBREW_NO_DELEGATE
             export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 \
                    HOMEBREW_NO_COLOR=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_ANALYTICS=1
             "$brew_bin" "$@" ); }

fb_prefix="$fb_dir/prefix"
ab_prefix="$ab_dir/prefix"

# The Ruby prefix doubles as brew's own git checkout, so only the installed
# state may be removed: kegs, opt records, linked records and the symlinks the
# link step created. brew's own files are never symlinks.
reset_installed() {
  local prefix="$1"
  rm -rf "$prefix/Cellar" "$prefix/opt"
  rm -rf "$prefix/var/homebrew/linked" "$prefix/var/homebrew/pinned" "$prefix/var/homebrew/locks"
  mkdir -p "$prefix/Cellar" "$prefix/opt" \
           "$prefix/var/homebrew/linked" "$prefix/var/homebrew/pinned" "$prefix/var/homebrew/locks"
  local d
  for d in bin sbin etc include lib share Frameworks; do
    [[ -d "$prefix/$d" ]] || continue
    find "$prefix/$d" -type l -delete 2>/dev/null
  done
}

echo "==> fastbrew sandbox: $fb_dir"
echo "==> brew sandbox:     $ab_dir"
if [[ -z "$keep" ]]; then
  [[ -d "$fb_prefix" ]] || run_fb --prefix >/dev/null 2>&1
  reset_installed "$fb_prefix"
  reset_installed "$ab_prefix"
fi
if [[ ${#fb_prefix} -gt 64 ]]; then
  echo "Warning: the fastbrew prefix is ${#fb_prefix} bytes; fixed-cellar bottles cannot be poured into it." >&2
fi

echo "==> installing with fastbrew: ${formulae[*]}"
run_fb install "${formulae[@]}" >"$out_dir/fb-install.log" 2>&1 || {
  echo "fastbrew install failed:"; tail -20 "$out_dir/fb-install.log"; }
echo "==> installing with brew: ${formulae[*]}"
run_ab install "${formulae[@]}" >"$out_dir/ab-install.log" 2>&1 || {
  echo "brew install failed:"; tail -20 "$out_dir/ab-install.log"; }

# --------------------------------------------------------------------------
# Normalizers.
# --------------------------------------------------------------------------
# Replace a prefix with @@PREFIX@@ and mask the byte counts, timestamps and
# character counts that legitimately differ between two prefixes.
normalize() {
  local prefix="$1"
  sed -E -e "s|$prefix|@@PREFIX@@|g" \
         -e "s#, [0-9]+(\.[0-9]+)?(KB|MB|GB|B)\)#, @@SIZE@@)#g" \
         -e "s#\([0-9]+(\.[0-9]+)?(KB|MB|GB|B)\)#(@@SIZE@@)#g" \
         -e "s#on [0-9]{4}-[0-9]{2}-[0-9]{2} at [0-9:]+#on @@TIME@@#g"
}

# One line per symlink: "<path relative to the prefix> -> <target>".
symlinks() {
  local prefix="$1" d
  for d in bin sbin etc include lib share Frameworks opt var/homebrew/linked; do
    [[ -d "$prefix/$d" ]] || continue
    find "$prefix/$d" -type l 2>/dev/null
  done | while read -r link; do
    printf '%s -> %s\n' "${link#"$prefix"/}" "$(readlink "$link")"
  done | LC_ALL=C sort
}

receipts() {
  local prefix="$1"
  find "$prefix/Cellar" -name INSTALL_RECEIPT.json 2>/dev/null | LC_ALL=C sort | while read -r r; do
    printf '=== %s\n' "${r#"$prefix"/Cellar/}"
    python3 "$repo_root/scripts/lib/normalize-receipt.py" "$r"
  done
}

report() {
  local what="$1" a="$2" b="$3"
  if diff -u "$a" "$b" >"$out_dir/diff.txt"; then
    echo "  ok   $what"
  else
    echo "  DIFF $what"
    sed -e '1,2d' "$out_dir/diff.txt" | sed 's/^/       /'
    status=1
  fi
}

status=0
echo "==> diffing"

symlinks "$fb_prefix" >"$out_dir/fb-links"
symlinks "$ab_prefix" >"$out_dir/ab-links"
report "symlinks" "$out_dir/fb-links" "$out_dir/ab-links"

receipts "$fb_prefix" >"$out_dir/fb-receipts"
receipts "$ab_prefix" >"$out_dir/ab-receipts"
report "receipts" "$out_dir/fb-receipts" "$out_dir/ab-receipts"

compare_command() {
  local label="$1"; shift
  run_fb "$@" 2>&1 | normalize "$fb_prefix" >"$out_dir/fb-cmd"
  run_ab "$@" 2>&1 | grep -v '^Warning: Your Homebrew.s prefix is not' \
    | normalize "$ab_prefix" >"$out_dir/ab-cmd"
  report "$label" "$out_dir/fb-cmd" "$out_dir/ab-cmd"
}

compare_command "list --versions" list --versions
compare_command "leaves" leaves
compare_command "deps --tree ${formulae[*]}" deps --tree "${formulae[@]}"
for f in "${formulae[@]}"; do
  compare_command "info $f" info "$f"
  compare_command "uses --installed $f" uses --installed "$f"
done

exit "$status"
