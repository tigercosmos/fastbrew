#!/bin/sh
# Install the latest fastbrew release binary.
#
#   curl -fsSL https://raw.githubusercontent.com/tigercosmos/fastbrew/master/install.sh | sh
#   wget -qO- https://raw.githubusercontent.com/tigercosmos/fastbrew/master/install.sh | sh
#
# Environment:
#   FASTBREW_VERSION   release tag to install (default: latest)
#   FASTBREW_BINDIR    where to put the binary (default: first writable
#                      directory on PATH among ~/.cargo/bin, ~/.local/bin,
#                      /opt/homebrew/bin, /usr/local/bin; else ~/.local/bin)
#
# The download is checked against the release's SHA256SUMS, and, when the
# GitHub CLI is installed, its build provenance attestation is verified.
set -eu

REPO="tigercosmos/fastbrew"
ASSET="fastbrew-aarch64-apple-darwin"

die() { echo "install.sh: $*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "fastbrew runs on macOS only"
[ "$(uname -m)" = "arm64" ] || die "prebuilt binaries are for Apple Silicon; build from source with cargo instead"

version="${FASTBREW_VERSION:-latest}"
if [ "$version" = "latest" ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$version"
fi

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    die "need curl or wget"
  fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $ASSET.tar.gz ($version)..."
fetch "$base/$ASSET.tar.gz" "$tmp/$ASSET.tar.gz"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"

(cd "$tmp" && grep " $ASSET.tar.gz\$" SHA256SUMS | shasum -a 256 -c - >/dev/null) \
  || die "SHA256 mismatch for $ASSET.tar.gz"
echo "Checksum verified."

if command -v gh >/dev/null 2>&1 && [ -z "${FASTBREW_SKIP_ATTESTATION:-}" ]; then
  if gh attestation verify "$tmp/$ASSET.tar.gz" --repo "$REPO" >/dev/null 2>&1; then
    echo "Build provenance verified (GitHub attestation)."
  else
    echo "Note: could not verify the build provenance with 'gh attestation verify'; the checksum still matched." >&2
  fi
fi

tar -C "$tmp" -xzf "$tmp/$ASSET.tar.gz"

bindir="${FASTBREW_BINDIR:-}"
if [ -z "$bindir" ]; then
  for d in "$HOME/.cargo/bin" "$HOME/.local/bin" /opt/homebrew/bin /usr/local/bin; do
    case ":$PATH:" in
      *":$d:"*) if [ -d "$d" ] && [ -w "$d" ]; then bindir="$d"; break; fi ;;
    esac
  done
fi
if [ -z "$bindir" ]; then
  bindir="$HOME/.local/bin"
  mkdir -p "$bindir"
fi

install -m 755 "$tmp/fastbrew" "$bindir/fastbrew"
echo "Installed $bindir/fastbrew"
case ":$PATH:" in
  *":$bindir:"*) ;;
  *) echo "Add $bindir to your PATH, for example: export PATH=\"$bindir:\$PATH\"" ;;
esac
"$bindir/fastbrew" --version
