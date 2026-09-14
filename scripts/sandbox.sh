#!/usr/bin/env bash
# Isolated Homebrew-compatible prefix for testing fastbrew.
#
# Usage:
#   scripts/sandbox.sh create [DIR]   create (or recreate) the sandbox tree
#   scripts/sandbox.sh env [DIR]      print `export` lines for the sandbox
#   scripts/sandbox.sh run [DIR] -- CMD...   run CMD inside the sandbox env
#   scripts/sandbox.sh test [DIR] [cargo test args]   fresh sandbox + integration tests
#   scripts/sandbox.sh brew [DIR]     clone the Ruby Homebrew into the sandbox prefix (for A/B benchmarks)
#   scripts/sandbox.sh destroy [DIR]  remove the sandbox
#
# DIR defaults to $FASTBREW_SANDBOX or <repo>/target/sandbox. The prefix is
# DIR/prefix. Nothing here touches /opt/homebrew, /usr/local or the user's
# Homebrew cache; the host API cache is only read (copied) to seed the sandbox.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cmd="${1:-}"
shift || true

# An optional DIR is recognised only when it looks like a path; anything else
# (e.g. a cargo test filter after `test`) is passed through in "$@".
dir=""
case "${1:-}" in
  --) shift ;;
  */*|.|..) dir="$1"; shift ;;
esac
dir="${dir:-${FASTBREW_SANDBOX:-$repo_root/target/sandbox}}"
dir="$(cd "$(dirname "$dir")" 2>/dev/null && pwd)/$(basename "$dir")"

forbidden() {
  case "$1" in
    /opt/homebrew|/opt/homebrew/*|/usr/local|/usr/local/*|/home/linuxbrew/.linuxbrew|/home/linuxbrew/.linuxbrew/*|"$HOME/Library/Caches/Homebrew"*)
      echo "sandbox.sh: refusing to use $1 (host Homebrew location)" >&2
      exit 2 ;;
  esac
}
forbidden "$dir"

prefix="$dir/prefix"
cache="$dir/cache"
home="$dir/home"
bottle_tag() {
  local arch os major name
  arch="$(uname -m)"; [[ "$arch" == "arm64" ]] || arch="x86_64"
  os="$(sw_vers -productVersion 2>/dev/null || echo 0)"
  major="${os%%.*}"
  case "$major" in
    27) name=golden_gate ;; 26) name=tahoe ;; 15) name=sequoia ;; 14) name=sonoma ;;
    13) name=ventura ;; 12) name=monterey ;; 11) name=big_sur ;; *) name=unknown ;;
  esac
  if [[ "$arch" == "arm64" ]]; then echo "arm64_${name}"; else echo "${name}"; fi
}

print_env() {
  # Overriding HOME must not break the Rust toolchain lookup.
  cat <<EOV
export RUSTUP_HOME="\${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="\${CARGO_HOME:-$HOME/.cargo}"
export HOMEBREW_PREFIX="$prefix"
export HOMEBREW_CELLAR="$prefix/Cellar"
export HOMEBREW_REPOSITORY="$prefix"
export HOMEBREW_LIBRARY="$prefix/Library"
export HOMEBREW_CACHE="$cache"
export HOMEBREW_LOGS="$dir/logs"
export HOMEBREW_TEMP="$dir/tmp"
export HOME="$home"
export HOMEBREW_CASK_OPTS="--appdir=$home/Applications --fontdir=$home/Library/Fonts"
export HOMEBREW_NO_AUTO_UPDATE=1
export HOMEBREW_NO_ANALYTICS=1
export HOMEBREW_NO_ENV_HINTS=1
export HOMEBREW_NO_INSTALL_CLEANUP=1
export FASTBREW_REQUIRE_SANDBOX=1
export FASTBREW_SANDBOX="$dir"
export PATH="$prefix/bin:$prefix/sbin:\$PATH"
EOV
}

create() {
  rm -rf "$dir"
  mkdir -p "$prefix"/{bin,sbin,etc,include,lib,share,Frameworks,Cellar,opt,Caskroom,Library/Taps} \
           "$prefix"/var/homebrew/{linked,pinned,pinned_casks,locks} \
           "$cache"/{api/internal,downloads,fastbrew} "$dir"/{logs,tmp} \
           "$home"/{Applications,Library/Fonts,Library/LaunchAgents,Library/Caches}
  # Seed the API cache from the host (read-only copy) to avoid a 15 MB download per sandbox.
  local tag host_api
  tag="$(bottle_tag)"
  host_api="$HOME/Library/Caches/Homebrew/api/internal/packages.${tag}.jws.json"
  if [[ -f "$host_api" ]]; then
    cp "$host_api" "$cache/api/internal/packages.${tag}.jws.json"
    touch -r "$host_api" "$cache/api/internal/packages.${tag}.jws.json"
  fi
  print_env > "$dir/env.sh"
  echo "sandbox created at $dir (prefix $prefix, bottle tag $tag)"
}

case "$cmd" in
  create) create ;;
  env) [[ -d "$prefix" ]] || create >/dev/null; print_env ;;
  run)
    [[ -d "$prefix" ]] || create >/dev/null
    [[ "${1:-}" == "--" ]] && shift
    eval "$(print_env)"
    exec "$@" ;;
  test)
    create
    eval "$(print_env)"
    cd "$repo_root"
    exec cargo test --test '*' -- --test-threads=1 "$@" ;;
  brew)
    [[ -d "$prefix" ]] || create >/dev/null
    if [[ -x "$prefix/bin/brew" ]]; then echo "brew already present at $prefix/bin/brew"; exit 0; fi
    tmp="$(mktemp -d "$dir/brew-clone.XXXX")"
    git clone --depth 1 https://github.com/Homebrew/brew "$tmp/brew"
    cp -R "$tmp/brew/." "$prefix/"
    rm -rf "$tmp"
    echo "Ruby Homebrew cloned into $prefix; run with: scripts/sandbox.sh run -- brew --version" ;;
  destroy) rm -rf "$dir"; echo "removed $dir" ;;
  *) sed -n '2,13p' "${BASH_SOURCE[0]}"; exit 1 ;;
esac
