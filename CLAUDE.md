# fastbrew

Rust drop-in replacement for the Homebrew `brew` command. Read `docs/DESIGN.md`
(architecture, pipelines, command coverage) and `docs/COMPAT.md` (exact on-disk
formats, network protocols, output formats) before changing anything.

## Hard rules

1. Never run fastbrew or `brew` against the host Homebrew. The host prefix
   `/opt/homebrew` and the host cache `~/Library/Caches/Homebrew` are
   read-only reference material. Every test, benchmark and manual run goes
   through `scripts/sandbox.sh`, which creates an isolated prefix, cache, HOME
   and app directory. Tests set `FASTBREW_REQUIRE_SANDBOX=1`; `config` refuses
   the standard prefixes when it is set.
2. Reading Homebrew's Ruby at `/opt/homebrew/Library/Homebrew` is the way to
   settle any behavior question. Port behavior, do not guess it.
3. Byte-compatible artifacts: receipts, symlinks, cache file names, Caskroom
   metadata. Homebrew must be able to manage what fastbrew installed.
4. No network in unit tests. Integration tests that need the network check
   `FASTBREW_TEST_NETWORK=1` and skip otherwise.
5. Speed is a feature: no JSON parsing of the 15 MB API file on the hot path,
   no process spawning in read-only commands, parallelize downloads and
   extraction.

## Workflow

- `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` must pass.
- `cargo test` runs unit tests; `scripts/sandbox.sh test` runs integration
  tests inside a fresh sandbox.
- Commit small, focused changes with imperative subjects.
- Keep `docs/COMPAT.md` correct: when you discover a behavior difference from
  Homebrew, fix the doc in the same change.
