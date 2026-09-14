# fastbrew

A fast Rust reimplementation of the Homebrew `brew` command for macOS.

fastbrew shares Homebrew's prefix, Cellar, receipts, JSON API and bottle
registry, so it can manage the same installation as `brew` and the two can be
used interchangeably. Read-only commands (`info`, `search`, `deps`, `list`,
`outdated`) answer from a memory-mapped index in a few milliseconds instead of
booting Ruby; installs download manifests and bottles concurrently and extract
and relocate them in parallel.

Common commands are native: `install`, `reinstall`, `upgrade`, `uninstall`,
`autoremove`, `cleanup`, `link`/`unlink`, `pin`/`unpin`, `info`, `search`,
`deps`, `uses`, `leaves`, `list`, `outdated`, `update`, `tap`/`untap`,
`services`, and casks. Paths that need the Ruby DSL (source builds, `--HEAD`,
developer commands, `brew bundle`) are delegated to an installed `brew`.

## Status

Working end to end on Apple Silicon macOS: formula install/upgrade/uninstall
from bottles (with relocation and code signing), casks (dmg/zip/pkg
containers, all common artifact kinds), taps, services, update, cleanup,
pin, and every common query command. Kegs and Caskroom entries are
byte-compatible with Homebrew 6: `brew` can list, upgrade and uninstall what
fastbrew installed and vice versa. Linux is not supported yet.

See `docs/DESIGN.md` for the architecture and `docs/COMPAT.md` for the
Homebrew formats fastbrew reproduces.

## Speed

Measured with `scripts/bench.sh` on an M-series Mac, both tools warm and
inside the same sandbox prefix (Homebrew 6, portable Ruby):

| command | brew | fastbrew |
|---|---|---|
| `info jq` | 348 ms | 4.4 ms |
| `search --desc json` | 330 ms | 6.8 ms |
| `deps --tree jq` | 327 ms | 4.2 ms |
| `outdated` | 275 ms | 4.1 ms |
| `uses --installed openssl@3` | 311 ms | 4.1 ms |
| `search ripgrep` | 394 ms | 10.2 ms |
| `install jq` (cold cache, downloads included) | 1.15 s | 0.55 s |
| `install jq` (bottles cached) | 0.47 s | 0.03 s |
| `uninstall jq oniguruma` | 0.38 s | under 0.01 s |

The 15 MB package index is parsed once per `update` into a memory-mapped
file; every query afterwards is a few microseconds of lookups.

## Installation

Requirements: macOS on Apple Silicon with Homebrew already installed.
fastbrew manages Homebrew's own prefix (`/opt/homebrew`) and hands the
commands it does not implement to `brew`, so Homebrew stays installed next
to it.

### One-line installer

```sh
curl -fsSL https://raw.githubusercontent.com/tigercosmos/fastbrew/master/install.sh | sh
# or
wget -qO- https://raw.githubusercontent.com/tigercosmos/fastbrew/master/install.sh | sh
```

The script downloads the latest release, checks it against the release's
`SHA256SUMS`, verifies the GitHub build-provenance attestation when the
`gh` CLI is available, and installs `fastbrew` into the first writable
directory on your PATH (`FASTBREW_BINDIR` overrides; `FASTBREW_VERSION`
selects a tag).

### Prebuilt binary by hand

Every release at https://github.com/tigercosmos/fastbrew/releases ships:

- `fastbrew-aarch64-apple-darwin.tar.gz`, the binary in a tarball
- `fastbrew-aarch64-apple-darwin`, the bare binary
- `SHA256SUMS`

```sh
curl -fsSL https://github.com/tigercosmos/fastbrew/releases/latest/download/fastbrew-aarch64-apple-darwin.tar.gz | tar xz
install -m 755 fastbrew ~/.local/bin/fastbrew          # any directory on your PATH
```

Releases are built by the `Release` GitHub Actions workflow from the tagged
commit and carry a signed build-provenance attestation (SLSA, via Sigstore).
To verify a download came from that workflow:

```sh
gh attestation verify fastbrew-aarch64-apple-darwin.tar.gz --repo tigercosmos/fastbrew
```

### From source

Needs Rust 1.90 or newer (`rustup` from https://rustup.rs):

```sh
cargo install --git https://github.com/tigercosmos/fastbrew --locked
```

This places `fastbrew` in `~/.cargo/bin`, which `rustup` adds to your PATH.
From a checkout, `cargo install --path . --locked` does the same.

### Use it in place of brew

`fastbrew` accepts the same commands and flags as `brew`, so you can alias
it in your shell:

```sh
echo 'alias brew=fastbrew' >> ~/.zshrc
```

Commands fastbrew does not implement (source builds, `--HEAD`, `bundle`,
developer commands) are passed to the real `brew` automatically. To remove
fastbrew, delete the binary; it keeps no state outside Homebrew's cache
directory (`~/Library/Caches/Homebrew/fastbrew`).

### Building and installing from a checkout

```sh
git clone https://github.com/tigercosmos/fastbrew.git
cd fastbrew
make build       # release build into target/release/fastbrew
make install     # copies it into the first writable bin directory on your PATH
```

`make install` picks `~/.cargo/bin`, `~/.local/bin`, `/opt/homebrew/bin` or
`/usr/local/bin`, whichever comes first among those that exist, are on your
PATH and are writable. Pass `BINDIR=/some/dir` to choose. `make uninstall`
removes the binary; `make test`, `make check` and `make bench` run the test
suite, lints and the A/B benchmark.

## Testing

All tests run inside an isolated sandbox prefix; nothing touches the
machine's Homebrew installation:

```sh
cargo test                       # unit tests
scripts/sandbox.sh test          # integration tests in a fresh sandbox
FASTBREW_TEST_NETWORK=1 scripts/sandbox.sh test   # include tests that download bottles
scripts/bench.sh                 # A/B timings against a sandboxed Ruby Homebrew
```
