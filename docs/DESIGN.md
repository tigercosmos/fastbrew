# fastbrew design

fastbrew is a Rust reimplementation of the Homebrew client. It is a drop-in
replacement for the `brew` command on macOS (Apple Silicon first, Intel and
Linux later) that shares Homebrew's on-disk layout, its JSON API, and its
bottle registry. The goal is Homebrew-level feature coverage for the commands
people run every day, at a fraction of Homebrew's latency.

Homebrew's own Ruby implementation lives at `/opt/homebrew/Library/Homebrew`
on the development machine. It is the reference: when this document and the
Ruby disagree, the Ruby wins, and this document must be corrected. Reading
that source is encouraged; running `brew` against the host prefix is not (see
`CLAUDE.md`).

## 1. Goals and non-goals

Goals:

1. Speed. Cold-start read-only commands (`info`, `search`, `deps`, `list`,
   `outdated`) complete in under 40 ms. Installing an already-downloaded
   bottle completes in well under a second. Network-bound work is limited by
   bandwidth, not by the tool: manifests and blobs download concurrently.
2. Compatibility. Kegs installed by fastbrew are indistinguishable from kegs
   installed by Homebrew: same `INSTALL_RECEIPT.json`, same symlinks, same
   `opt` and `var/homebrew/linked` records, same Caskroom metadata. Homebrew
   can upgrade or uninstall what fastbrew installed and vice versa.
3. Coverage. Every common Homebrew command works natively (section 5). Rare
   paths that need the Ruby DSL (source builds, Ruby `post_install` blocks in
   third-party taps, `brew bundle`, developer commands) delegate to the
   installed `brew` when one exists.

Non-goals:

- Reimplementing the formula Ruby DSL or building from source.
- Homebrew developer commands (`brew bottle`, `brew audit`, `brew pr-*`).
- Linux support in the first release (the design keeps it possible).

## 2. Baseline

Measured on the development machine (M-series, Homebrew 6.0.22, warm caches,
`HOMEBREW_NO_AUTO_UPDATE=1`):

| command | brew |
|---|---|
| `brew info jq` | 0.58 s |
| `brew deps --tree --installed` | 0.81 s |
| `brew uses --installed openssl@3` | 0.79 s |
| `brew search --desc json` | 0.33 s |
| `brew outdated` | 0.70 s (6.4 s with auto-update) |
| `brew services list` | 0.54 s |

fastbrew targets: 40 ms or less for all of the above.

## 3. Architecture

Single Cargo crate `fastbrew` with a library (`src/lib.rs`) and a thin binary
(`src/main.rs`). Modules and their responsibilities:

| module | responsibility |
|---|---|
| `config` | Resolve `HOMEBREW_*` environment and paths (prefix, cellar, cache, temp, logs, taps dir). Sandbox guard. |
| `platform` | macOS version, CPU arch, bottle tag (`arm64_tahoe`), padded prefix constants. |
| `api` | Download the internal packages JWS, verify PS512 signature, cache it, build and memory-map the fast index. |
| `model` | Typed views of internal-API formula and cask entries, `INSTALL_RECEIPT.json`, cask receipts and config. |
| `version` | Homebrew `Version`/`PkgVersion` tokenizer and comparison (exact port). |
| `resolve` | Name resolution: aliases, renames, tap migrations, `tap/repo/name`, installed-only lookups. |
| `deps` | Dependency graph: runtime vs build/test, `uses_from_macos` bounds, topological order, `uses`, `leaves`, autoremove set. |
| `keg` | Keg discovery, receipts, link/unlink/optlink, linked and pinned records, locks. |
| `bottle` | Manifest fetch, blob download, cache naming, tar extraction, relocation (text, Mach-O, build prefix), codesign. |
| `ops` | High-level operations: install, reinstall, upgrade, uninstall, outdated, cleanup, pin, link, postinstall. |
| `cask` | Cask engine: download, unpack (dmg/zip/tar/pkg/naked), artifacts, uninstall/zap, Caskroom metadata. |
| `services` | launchd plist generation from API service data, `services list/start/stop/restart/run/info`. |
| `tap` | Tap clone/update/remove, formula and cask file enumeration. |
| `rubylite` | Metadata extraction from Ruby formula/cask files in third-party taps (no evaluation). |
| `delegate` | Locate and exec the Ruby `brew` for unsupported paths. |
| `update` | `fastbrew update`: conditional API fetch, tap `git pull`, change report, auto-update policy. |
| `output` | Homebrew-style terminal output (`==>`, `Warning:`, `Error:`, colors, TTY detection, JSON writers). |
| `cli` | clap command tree mirroring Homebrew's flags. |

Concurrency model: blocking `reqwest` (rustls) for HTTP, `rayon` for parallel
downloads, extraction, relocation and codesigning. No async runtime in the
hot path.

## 4. Data sources

### 4.1 Package metadata

Source: `https://formulae.brew.sh/api/internal/packages.<bottle_tag>.jws.json`
(about 15 MB, gzip on the wire). Format and verification are documented in
`docs/COMPAT.md` section 1. fastbrew stores the raw file at exactly the path
Homebrew uses (`$HOMEBREW_CACHE/api/internal/packages.<tag>.jws.json`) so the
two tools share one download.

Fast index: on first use after a download, fastbrew parses the payload once
and writes `$HOMEBREW_CACHE/fastbrew/index/<tag>-<fingerprint>.fbi`, where the
fingerprint is the source file's size and mtime. The index is a zero-copy,
memory-mappable archive containing every formula and cask entry plus the
alias, rename and migration maps and a lower-cased name and description table
for search. Every read-only command loads the index with `mmap` and never
parses JSON. Loading must cost under 5 ms; a full scan of all entries must
cost under 15 ms. The implementation may use `rkyv` or a hand-rolled format;
it must detect version mismatches and rebuild.

Third-party taps: formula and cask metadata comes from `rubylite` parsing of
the tap's `.rb` files, cached at
`$HOMEBREW_CACHE/fastbrew/taps/<user>-<repo>.json` and keyed per file by its
size and mtime plus the host bottle tag. A command re-parses only the files
that changed; files `rubylite` cannot read are cached as failures with their
reason so the command can delegate without parsing them again. Official taps
are never read here: the API is their source of truth, exactly as
`Formulary`'s `FromAPILoader` running before any tap loader.

### 4.2 Installed state

Read from the filesystem exactly as Homebrew does: `$HOMEBREW_CELLAR/<name>/<version>/INSTALL_RECEIPT.json`, `$HOMEBREW_PREFIX/opt/<name>`, `$HOMEBREW_PREFIX/var/homebrew/{linked,pinned,pinned_casks,locks}`, `$HOMEBREW_PREFIX/Caskroom/<token>/.metadata`. No private database of installed state is kept; nothing must go stale.

### 4.3 Bottles

OCI manifests and blobs on `ghcr.io` (see `docs/COMPAT.md` section 2). Cache
file names match Homebrew's so both tools share downloads.

## 5. Command coverage

Native (first release), with Homebrew's flags where they matter:

- Query: `info` (incl. `--json=v2`, `--cask`, `--installed`), `search` (name, `--desc`, `/regex/`, `--formula`, `--cask`), `desc`, `home`, `list` (`--versions`, `--pinned`, `--formula`, `--cask`, `--full-name`, `-1`, `--installed-on-request`, `--installed-as-dependency`, `--json`), `deps` (`--tree`, `--installed`, `--include-build`, `--include-test`, `--include-optional`, `-n`, `--for-each`, `--direct`), `uses` (`--installed`, `--recursive`), `leaves`, `outdated` (`--formula`, `--cask`, `--greedy`, `--json`, `-v`), `missing`, `options` (prints nothing for API formulae), `commands`, `completions` (`link`, `unlink`, `state`), `help [command]`, `doctor` (the native subset below), `config`, `--prefix [formula]`, `--cellar [formula]`, `--cache [formula]`, `--repository`, `--caskroom`, `--taps`, `--version`, `shellenv`, `which-formula`.
- Third-party taps: `info`, `search`, `deps`, `uses`, `outdated` and
  `list --full-name` cover tap packages, by `user/repo/name` and by a bare
  name that only an installed tap provides (`Formulary`'s `FromNameLoader`:
  core first, then every installed tap, ambiguity listing the candidates).
- Mutating: `install` (bottles; `--cask`; `--only-dependencies`; `--ignore-dependencies`; `--force`; `--dry-run`; `--overwrite`; `--skip-post-install`; `--quiet`; `--verbose`), `reinstall`, `upgrade` (all or named; `--cask`; `--greedy`; `--dry-run`), `uninstall`/`remove`/`rm` (`--force`, `--ignore-dependencies`, `--zap` for casks), `autoremove`, `cleanup` (`-n`, `--prune=days`, `-s`), `link`/`unlink` (`--overwrite`, `--force`, `-n`), `pin`/`unpin`, `postinstall`, `fetch`, `update` (`--auto-update`, `--quiet`), `tap`/`untap`/`tap-info`, `services` (`list`, `info`, `start`, `stop`, `restart`, `run`, `kill`, `cleanup`, `--json`).
- Casks: `install --cask`, `uninstall --cask [--zap]`, `upgrade --cask`, `outdated --cask`, `info --cask`, `list --cask`, `reinstall --cask`, `fetch --cask`. Artifacts: `app`, `binary`, `manpage`, `bash_completion`, `zsh_completion`, `fish_completion`, `font`, `pkg`, `installer` (script), `suite`, `artifact`, `qlplugin`, `prefpane`, `screen_saver`, `service`, `input_method`, `dictionary`, `command_wrapper`, `uninstall` directives, `zap`, `preflight_steps`/`postflight_steps` (declarative). Containers: dmg, zip, tar.*, pkg, naked, nested.

`doctor` runs a native subset (`check_for_broken_symlinks`,
`check_for_unlinked_but_not_keg_only` and a fastbrew-specific
`check_missing_opt_links`), lists them with `--list-checks`, and hands any
other named check to `brew doctor`. Findings print Homebrew's preamble and
exit 1; a clean prefix prints `Your system is ready to brew.`

Delegated to `brew` (with a one-line notice on stderr):

- `install --build-from-source`, `install --HEAD`, install of a formula with no bottle for this platform, install of a third-party tap formula whose metadata `rubylite` cannot extract, `postinstall` for a formula with a Ruby `post_install` (third-party taps), `bundle`, `test`, `edit`, `create`, `livecheck`, `--env` (it needs the full build environment), `link --cask`/`unlink --cask`, `pin --cask`/`unpin --cask`, every `dev-cmd`, and any command fastbrew does not know.

If no `brew` is available the delegated command fails with an explanation and
a pointer to the Homebrew installer.

## 6. Install pipeline (formulae)

Order of operations for `fastbrew install a b c`:

1. Auto-update check (section 9), then resolve each name (section 4.1, `resolve`).
2. Build the dependency closure with `deps`: runtime deps only, honoring `uses_from_macos` bounds against the running macOS, skipping already-installed and up-to-date kegs. Detect `conflicts` against installed and linked kegs. Refuse `disabled` formulae; warn on `deprecated`. Refuse `pour_bottle_args` `only_if: :default_prefix` when the prefix is not the default.
3. Take formula locks for everything in the plan (`var/homebrew/locks/<name>.formula.lock`, `flock`).
4. Fetch all manifests concurrently, then all blobs concurrently (default 8 in flight, `HOMEBREW_DOWNLOAD_CONCURRENCY`). Verify blob sha256 against the API `bottle_checksum` while streaming. Reuse cached files.
5. Extract in dependency order (parents after dependencies), each into a temporary directory under the rack, then rename into `Cellar/<name>/<version>`. Extraction of independent formulae runs in parallel.
6. Write the receipt (`docs/COMPAT.md` section 3) merging the manifest's `sh.brew.tab` with install-time fields.
7. Relocate (`docs/COMPAT.md` section 4). Re-sign modified Mach-O files.
8. Relativize absolute symlinks pointing into the build prefix if it differs from ours.
9. `optlink`, then `link` unless keg-only (or `--no-link` semantics of keg-only), recording `var/homebrew/linked/<name>`. Conflicts abort the link (not the install) with Homebrew's message unless `--overwrite` or the path is listed in `link_overwrite_paths`.
10. Write service files (`homebrew.mxcl.<name>.plist` and `homebrew.<name>.service`) into the keg when `service_run_args` exists.
11. Install `etc` and `var` seed files into the prefix (`.default` suffix when the destination exists and differs).
12. Run declarative `post_install_steps` (section 7) unless `--skip-post-install`.
13. Update the receipt's `runtime_dependencies` from the final resolved graph and write it again.
14. Print caveats and the summary line `🍺  <keg path>: <n> files, <size>`.
15. Unless `HOMEBREW_NO_INSTALL_CLEANUP`, remove older kegs of the installed formulae (`cleanup` rules) and unreferenced cached bottles of those formulae.

Every step that writes is idempotent and safe to retry; failure after step 5
removes the partially installed keg unless the user passes `--keep-tmp`
equivalent debugging flags. Steps 4 and 5 are pipelined: a formula extracts as
soon as its own blob and its dependencies' kegs are ready.

## 7. Declarative post-install steps

Homebrew 6 serializes core formulae's `post_install` blocks as
`post_install_steps` (types observed: `run`, `mkdir_p`, `mkdir`, `touch`,
`symlink`, `copy`, `move`, `move_children`, `remove`, `link_dir`,
`link_children`, `write`, `inreplace`, `set_permissions`, `set_ownership`,
`init_data_dir`, `compile_gsettings_schemas`, `gtk_update_icon_cache`,
`gdk_pixbuf_query_loaders`, `gio_querymodules`, `update_mime_database`,
`update_desktop_database`, `install_gzipped_executable`,
`configure_gcc_runtime`, `configure_clang_system`, `configure_glibc_runtime`,
`configure_php`, `bootstrap_cpython`, `bootstrap_pypy`, `terminate_process`,
`change_dylib_id`, `warn`, `delete_keychain_certificate`). Each step carries
`guards` (`if_exists`, `unless_exists`, `on: macos|linux`) and path specs with
a `base` (`homebrew_prefix`, `prefix`, `opt_prefix`, `bin`, `lib`, `etc`,
`var`, `share`, `frameworks`, `libexec`, `pkgetc`, `pkgshare`, `home`, ...)
and template tokens (`{{version}}`, `{{version.major}}`,
`{{version.major_minor}}`, `{{name}}`, `{{HOMEBREW_PREFIX}}`, `{{arch}}`).
The executor is a port of `Library/Homebrew/install_steps.rb`. Unknown step
types abort post-install with a clear error and suggest `brew postinstall`.

## 8. Cask pipeline

1. Resolve token (renames, tap-qualified), check `depends_on` (`macos`
   comparators and version symbols, `arch`, `formula`, `cask`), conflicts,
   `disable_args`/`deprecate_args`.
2. Install formula and cask dependencies first.
3. Download with `url_kwargs` (`user_agent` including `:fake`, `referer`,
   `cookies`, `header`), verify `sha256` unless `:no_check`, cache under
   Homebrew's naming.
4. Stage into `Caskroom/<token>/<version>/` using the container type from the
   URL extension or `container_args` (`type: :naked` copies the download;
   `nested:` unpacks the inner file). dmg: `hdiutil attach -plist -nobrowse
   -readonly -mountrandom`, copy with `ditto`, `hdiutil detach`. zip: `ditto
   -x -k`. tar family: native. pkg: left in place for the `pkg` artifact.
5. Copy the quarantine attribute the way Homebrew does (`xattr` on staged
   files with the download's `com.apple.quarantine` value, no-translocation
   bit set).
6. Run artifacts in the API order: `preflight_steps`, moved artifacts (`app`,
   `suite`, `artifact`, `font`, `qlplugin`, `prefpane`, ...) into their
   configured directories (defaults in `docs/COMPAT.md` section 6, overridable
   by `HOMEBREW_CASK_OPTS` and `--appdir` etc.), symlinked artifacts
   (`binary`, `manpage`, completions) into the prefix, `pkg` via `sudo
   /usr/sbin/installer -pkg <file> -target /` (with `choices` XML), `installer
   script`, `postflight_steps`. After a moved artifact is moved, leave a
   symlink at the staged location pointing to the target, as Homebrew does.
7. Write `.metadata/<version>/<timestamp>/Casks/<token>.json`, `.metadata/config.json`, `.metadata/INSTALL_RECEIPT.json` with `uninstall_artifacts`.

Uninstall: run `uninstall` directives in Homebrew's order (`early_script`,
`launchctl`, `quit`, `signal`, `login_item`, `kext`, `script`, `pkgutil`,
`delete`, `trash`, `rmdir`), reverse the artifacts (move apps back and delete,
remove symlinks), purge the versioned Caskroom directory and metadata. `--zap`
additionally runs `zap` directives. Upgrade = install new version with the old
cask as predecessor, then uninstall the old version.

## 9. Update and auto-update

`fastbrew update`:

1. Conditional GET of the internal packages file using `If-Modified-Since`
   from the cached file's mtime (Homebrew uses `curl --time-cond`). On 304 the
   cache is fresh; touch its mtime. On 200 verify the JWS before replacing the
   file atomically, then rebuild the fast index.
2. `git -C <tap> fetch` and fast-forward each third-party tap concurrently,
   in parallel with step 1.
3. Report, as `ReporterHub#dump` does in Homebrew 6:
   `Updated N taps (user/repo, ...).` naming every tap whose `HEAD` moved
   (plus `homebrew/core` and `homebrew/cask` when the API file changed), then
   `==> New Formulae` (`name: description`, skipping installed ones),
   `==> New Casks` (only when a cask is installed), `==> Deleted Installed
   Formulae`, `==> Deleted Installed Casks`, `==> Outdated Formulae`,
   `==> Outdated Casks` and the `You have N outdated formulae installed.`
   summary. When nothing moved: `Already up-to-date.`; when something moved
   but no package changed: `No changes to formulae or casks.` Homebrew 6 no
   longer prints "Updated Formulae", "Renamed Formulae" or a plain "Deleted
   Formulae" section, and fastbrew follows it.

Auto-update runs before `install`, `upgrade`, `outdated` and `tap <name>`
(never a bare `tap`) unless `HOMEBREW_NO_AUTO_UPDATE` is set, when the last
check is older than `HOMEBREW_AUTO_UPDATE_SECS`. That default is 86400, or
300 when a `user/repo/name` argument names a third-party tap, whose metadata
the API does not carry (`utils/auto-update.sh`, `env_config.rb`). The
auto-update itself accepts a cached API file younger than
`HOMEBREW_API_AUTO_UPDATE_SECS` (default 450); an explicit `fastbrew update`
always revalidates. The report is headed `==> Auto-updated Homebrew!`, is
suppressed when `HOMEBREW_AUTO_UPDATE_QUIET` is set or nothing changed, and
skips the outdated listing before a bare `upgrade`/`outdated`, which print it
themselves.

## 10. Output and errors

- `==> Title` in bold blue on a TTY, plain otherwise. `Warning:` yellow,
  `Error:` red, both to stderr. Emoji summary line unless `HOMEBREW_NO_EMOJI`.
- `HOMEBREW_NO_COLOR`, `HOMEBREW_COLOR`, `NO_COLOR` respected.
- Exit codes: 0 success, 1 error. Unknown formula prints Homebrew's wording:
  `Error: No available formula with the name "x". Did you mean y?` with
  suggestions from the fast index (edit distance and prefix matches).
- `--json` outputs mirror Homebrew's schemas so scripts keep working.

## 11. Testing

- Unit tests next to each module. The `version` module ports Homebrew's
  `version_spec.rb` cases.
- Integration tests in `tests/` run only inside the sandbox created by
  `scripts/sandbox.sh` (they call it themselves and set the environment). They
  install small relocatable bottles (`hello`, `tree`, `jq` with `oniguruma`,
  `xz`), exercise link/unlink/pin/uninstall/upgrade paths with a frozen copy
  of the API file, and compare produced receipts and symlinks against golden
  expectations. Network tests are opt-in via `FASTBREW_TEST_NETWORK=1`.
- A/B benchmarks (`scripts/bench.sh`) compare fastbrew with a sandboxed Ruby
  Homebrew clone inside the same sandbox.
- Never run against the host `/opt/homebrew`. `config` refuses to operate on
  `/opt/homebrew`, `/usr/local` or `/home/linuxbrew/.linuxbrew` when
  `FASTBREW_REQUIRE_SANDBOX=1`, which every test sets.

## 12. Milestones

1. Foundation: config, platform, API fetch/verify/index, model, version,
   resolve, keg reading, read-only commands, output, CLI skeleton, sandbox.
2. Bottle install: fetch, extract, receipt, relocation (text, Mach-O,
   codesign, build prefix), link/unlink, services files, post-install steps,
   install/reinstall/upgrade/uninstall/cleanup/pin, update.
3. Casks: download, unpack, artifacts, uninstall/zap, metadata.
4. Services, taps and rubylite, delegation, `info --json`, benchmarks, docs.
