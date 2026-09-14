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

Under active development. See `docs/DESIGN.md` for the architecture and
`docs/COMPAT.md` for the Homebrew formats fastbrew reproduces.

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
