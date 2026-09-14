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
| `install jq` (downloads included) | about 4 s | 0.8 s |
| `install jq` (bottles cached) | about 2 s | 0.03 s |

The 15 MB package index is parsed once per `update` into a memory-mapped
file; every query afterwards is a few microseconds of lookups.

## Building

```sh
cargo build --release
target/release/fastbrew --version
```

## Testing

All tests run inside an isolated sandbox prefix; nothing touches the
machine's Homebrew installation:

```sh
cargo test                       # unit tests
scripts/sandbox.sh test          # integration tests in a fresh sandbox
FASTBREW_TEST_NETWORK=1 scripts/sandbox.sh test   # include tests that download bottles
scripts/bench.sh                 # A/B timings against a sandboxed Ruby Homebrew
```
