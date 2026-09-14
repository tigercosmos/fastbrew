# Homebrew compatibility reference

Facts fastbrew must reproduce, verified against Homebrew 6.0.22 on 2026-09-14.
Paths below use `$PREFIX` for `HOMEBREW_PREFIX` (default `/opt/homebrew` on
Apple Silicon, `/usr/local` on Intel, `/home/linuxbrew/.linuxbrew` on Linux),
`$CELLAR` for `$PREFIX/Cellar` and `$CACHE` for `HOMEBREW_CACHE` (default
`~/Library/Caches/Homebrew`; logs `~/Library/Logs/Homebrew`; temp
`/private/tmp`).

## 1. Package metadata API

### 1.1 Endpoint

`GET https://formulae.brew.sh/api/internal/packages.<bottle_tag>.jws.json`
(`HOMEBREW_API_DOMAIN` overrides the domain). The server sends `ETag`,
`Last-Modified`, `Cache-Control: max-age=600`, gzip when asked. Homebrew
fetches with `curl --compressed --time-cond <cached file>` and touches the
cached file's mtime after a successful revalidation. Cache path:
`$CACHE/api/internal/packages.<bottle_tag>.jws.json`. Homebrew also writes
sidecars `.payload` and `.payload.index` next to it; fastbrew must not depend
on them and must not break Homebrew by deleting them.

Other endpoints (same domain, same JWS wrapping unless noted):
`formula.jws.json` (35 MB, full v2 schema for all formulae),
`cask.jws.json`, `formula_tap_migrations.jws.json`,
`formula/<name>.json` and `cask/<token>.json` (plain JSON, v2 schema, used
for `info --json=v2`).

### 1.2 JWS wrapper and verification

```json
{"payload": "<JSON string>", "signatures": [{"protected": "<b64url>", "header": {"kid": "homebrew-1"}, "signature": "<b64url>"}]}
```

Pick the signature whose `header.kid` is `homebrew-1`. Decode `protected`
(base64url, no padding); it must be a JSON object with `"alg": "PS512"` and
`"b64": false` (`crit: ["b64"]`). Verify RSA-PSS with SHA-512, MGF1-SHA-512,
salt length equal to the digest length, over the bytes
`<protected_b64> + "." + <payload string exactly as it appears in the JSON>`
using the public key below (`Library/Homebrew/api/homebrew-1.pem`). A failed
verification must delete the cached file and abort with Homebrew's wording:
`Failed to verify integrity (<reason>) of: <url>` / `Potential MITM attempt
detected. Please run `brew update` and try again.`

```
-----BEGIN PUBLIC KEY-----
MIICIjANBgkqhkiG9w0BAQEFAAOCAg8AMIICCgKCAgEAyKoOYzp1rhwXISRi61BY
XBEr2PalSK8lEVOL2USy7mpy0OubOlFyujawyQcBcCn+uPOJ/WaK+POhNWcLLoiK
L2m8GViaQm7SMwdLKUXFgKSPHcG/1m6Vu+TNBKTfQqT60PjEYIrn5NW9ZrM0cUhK
REmsbeAMBevdSaW9UwY9iIhprrgovvT8SzKhF8ZOIZKXfJX4VNk0y/7VJYNuGGqH
3npxV7OKd4yTGRGqFcC9kJ84me3thiu0yqlOjASmfWIwIwcfp4j6BEM2LuqKd7yX
h51/O+MTthkuxV36moDKfdgdOFsvlCFkziaYLScCX9lOlmZHtOfJTAOXxTmM7qGr
wTGK0vhvTi8k9dBmH/dccredQBtPOfM/FEdeyakGLoTcDguiBS/4El3I2KtF6B2h
OGoBumR915/cI4drr5yPMduZ7gjs7ZEZnVkeVzic24TfUHpnOYzrhucNJtHMBDj9
6d1Gk82AhtuF9KlusLmCb6qXCWQSp/A4RZpN37E/p9q8rLp/7B/zp8X2TVvecPNy
BdMagdktdEqK7WPlYMcUp56JaOph8vqYoU+oGyCpWoLvcXFb75o4eefuu6Rs5SyM
c9JCCJ0DDFPjCRFnGPkvsKxFCzMFqH1jpWH0RQIrgmNVM5PO84iRH9YJsSPQzpMj
KvK/ZH4YgR9wNkBNagFo7lsCAwEAAQ==
-----END PUBLIC KEY-----
```

### 1.3 Payload schema

Top level:

```
metadata: {homebrew_version, bottle_tag, generated_at}
formulae: {name: FormulaEntry}            # 8598 entries
casks: {token: CaskEntry}                 # 7723 entries
formula_aliases: {alias: name}
formula_renames: {old: new}
cask_renames: {old: new}
formula_tap_git_head: "<sha>"
cask_tap_git_head: "<sha>"
formula_tap_migrations: {name: "user/repo"}
cask_tap_migrations: {token: "user/repo"}
```

Ruby symbols are serialized as strings with a leading colon (`":build"`,
`":any"`, `":sequoia"`). Keys of hashes that were symbols are `":date"` etc.
Optional keys are omitted, never null. Placeholders `$HOMEBREW_PREFIX`,
`$HOMEBREW_CELLAR` and `$HOME` (written `/$HOME` in some fields) appear in
caveats and service values; casks use `$APPDIR` and `$HOMEBREW_PREFIX`.

FormulaEntry (key: count of formulae carrying it):

```
desc 8598, homepage 8598, license 8598 (string, or object like {":any_of": [...]}; render like brew's SPDX formatter),
ruby_source_checksum 8598 (sha256 string), stable_version 8598,
stable_url_args 8598: [url] or [url, {":tag": .., ":revision": .., ":using": ..}],
stable_checksum 8336, head_url_args 4485,
bottle_checksum 8432 (sha256 of the bottle blob for THIS bottle_tag; absent = no bottle),
bottle_cellar 3804: ":any" (2788) or "/opt/homebrew/Cellar" (1016). ABSENT MEANS ":any_skip_relocation" (4794).
bottle_tag 1137: ":all" or an older tag like ":arm64_sequoia" when the bottle for this platform is served under another tag.
bottle_rebuild 1669 (integer > 0), revision 1190 (integer > 0), version_scheme 126,
stable_dependencies 7337: [ "name" | {"name": ":build"} | {"name": ":test"} | {"name": [":build", ":test"]} | {"name": ":optional"|":recommended"} ],
head_dependencies 7402 (same shape),
stable_uses_from_macos 1811: [ ["name"] | ["name", {":since": ":sequoia"}] | [{"name": ":build"}] | [{"name": ":build"}, {":since": ..}] ],
head_uses_from_macos 1819,
executables 7216: ["bin names"],
stable_patches 820, caveats 530 (string with $HOMEBREW_PREFIX placeholders),
conflicts 509: [ ["name", {":because": "reason"}] ],
deprecate_args 495: {":date": "YYYY-MM-DD", ":because": ":deprecated_upstream"|"text", ":replacement_formula"?: .., ":replacement_cask"?: ..},
disable_args 489 (same shape),
service_args 360: [ [":run_type", ":immediate"|":interval"|":cron"], [":working_dir", ".."], [":log_path", ".."], [":error_log_path", ".."], [":keep_alive", true|{":always": true}|{":successful_exit": bool}|{":crashed": bool}|{":path": ".."}], [":interval", n], [":cron", "..."], [":environment_variables", {..}], [":process_type", ":background"|..], [":require_root", bool], [":launch_only_once", bool], [":restart_delay", n], [":root_dir", ..], [":input_path", ..], [":macos_legacy_timers", bool], [":sockets", ..] ],
service_run_args 357: [ ["cmd", "arg", ...] ] or [ [{":macos": ["cmd", ...], ":linux": ["cmd", ...]}] ],
service_run_kwargs 3, service_name_args 3: [ {":macos": "custom.label", ":linux": "unit"} ],
keg_only_args 251: [":provided_by_macos"|":shadowed_by_macos"|":versioned_formula"|"free text reason", "optional explanation"],
aliases 249, versioned_formulae 218, oldnames 173,
post_install_steps 162 (see DESIGN.md section 7 and Library/Homebrew/install_steps.rb),
no_autobump_args 150, link_overwrite_paths 43 (globs relative to prefix),
pour_bottle_args 25: {":only_if": ":default_prefix"|":clt_installed"}
```

CaskEntry:

```
homepage, names: ["Display Name"], ruby_source_checksum: {":sha256": ".."}, ruby_source_path: "Casks/g/ghostty.rb",
tap_string: "homebrew/cask", version: "1.3.1" or "latest",
url_args: [url] or [url, {..}], url_kwargs 620: {":user_agent": "..."|":fake", ":referer": .., ":cookies": {..}, ":header": [..], ":using": ":post"|.., ":data": {..}, ":branch": .., ":only_path": ..},
sha256: "<hex>" or ":no_check",
raw_artifacts: [ [":app", ["Name.app"]] | [":app", ["Name.app", {":target": "Other.app"}]] | [":binary", ["path", {":target": "name"}]] | [":manpage", ["path"]] | [":bash_completion"|":zsh_completion"|":fish_completion", ["path"]] | [":font", ["file.ttf"]] | [":pkg", ["file.pkg", {":choices": [..], ":allow_untrusted": bool}]] | [":installer", {":script": {":executable": .., ":args": [..], ":sudo": bool}} | {":manual": "text"}] | [":suite", ["Dir"]] | [":artifact", ["src", {":target": "/abs/path"}]] | [":uninstall", {":quit": .., ":launchctl": .., ":signal": [[sig, id]], ":login_item": .., ":kext": .., ":script": {..}, ":pkgutil": .., ":delete": .., ":trash": .., ":rmdir": ..}] | [":zap", {same keys}] | [":preflight_steps"|":postflight_steps"|":uninstall_preflight_steps"|":uninstall_postflight_steps", {":steps": [..]}] | [":command_wrapper", ["name", {":executable": ".."}]] | [":generate_completions_from_executable", ["exe", "sub", {":shells": [..]}]] | [":qlplugin"|":prefpane"|":screen_saver"|":service"|":input_method"|":dictionary"|":colorpicker"|":mdimporter"|":vst_plugin"|":vst3_plugin"|":audio_unit_plugin"|":keyboard_layout", ["path"]] ],
desc 5079, depends_on_args 5012: {":macos": ":ventura" | {">=": [":ventura"]} | [":sonoma", ":sequoia"] , ":arch": ":arm64"|[..], ":cask": [..], ":formula": [..], ":maximum_macos": ..},
auto_updates 1953, disable_args 897, deprecate_args 294, caveats_rosetta 780, raw_caveats 212,
conflicts_with_args 344: {":cask": [..], ":formula": [..]},
container_args 40: {":nested": "inner.dmg"} | {":type": ":naked"|":dmg"|..},
renames 29, languages 28, language_variations 28
```

Artifact paths starting with `$APPDIR` refer to the resolved app directory.
A bare relative path is relative to the staged directory.

### 1.4 Bottle tag

`<arch>_<os_name>` with arch `arm64` (x86_64 macOS omits the arch prefix:
`sonoma`, not `x86_64_sonoma`). macOS names: 27 golden_gate, 26 tahoe, 15
sequoia, 14 sonoma, 13 ventura, 12 monterey, 11 big_sur. Linux: `arm64_linux`,
`x86_64_linux`. `:all` bottles are architecture independent. macOS version
comes from `/System/Library/CoreServices/SystemVersion.plist`
`ProductVersion` (or `sw_vers -productVersion`).

## 2. Bottle registry (ghcr.io)

- Root: `https://ghcr.io/v2/homebrew/core` (`HOMEBREW_BOTTLE_DOMAIN`). Third-party taps use `https://ghcr.io/v2/<user>/<repo-without-homebrew->`.
- Image name: formula name with `@` replaced by `/` and `+` by `x` (`python@3.14` -> `python/3.14`).
- Manifest tag: `<version>` for rebuild 0, `<version>-<rebuild>` otherwise, where version is `pkg_version` (`<stable_version>` or `<stable_version>_<revision>`). Example: `hello/manifests/2.12.3-1`, `jq/manifests/1.8.2-1`.
- Request: `GET /v2/homebrew/core/<image>/manifests/<tag>` with headers `Authorization: Bearer QQ==` and `Accept: application/vnd.oci.image.index.v1+json`. Response is an OCI image index. Select the entry whose `annotations["org.opencontainers.image.ref.name"]` equals `<version>.<bottle_tag>` (rebuild 0) or `<version>.<bottle_tag>.<rebuild>`; for `:all` bottles the tag part is `all`. When the API says `bottle_tag` is another tag, select that tag.
- Per-platform annotations: `sh.brew.bottle.digest` (sha256 of the blob; must equal the API `bottle_checksum`), `sh.brew.bottle.size`, `sh.brew.bottle.installed_size`, `sh.brew.license`, `sh.brew.path_exec_files`, `sh.brew.sbom.supplement`, `sh.brew.tab` (JSON string; keys `homebrew_version`, `changed_files`, `linkage_files` (optional), `binary_relocation_files` (optional), `padded_prefix` (optional bool), `built_prefix` (optional), `source_modified_time`, `compiler`, `runtime_dependencies`, `arch`, `built_on`).
- Blob: `GET /v2/homebrew/core/<image>/blobs/sha256:<digest>` with the same Authorization header; follows a redirect to a CDN. Body is a gzip tarball whose entries start with `<name>/<version>/`.
- `HOMEBREW_GITHUB_PACKAGES_TOKEN`/`_USER` may replace the anonymous token; `HOMEBREW_ARTIFACT_DOMAIN` rewrites the ghcr host.

Cache naming (shared with Homebrew):

- Blob: `$CACHE/downloads/<sha256 hex of the blob URL>--<name>--<pkg_version>.<bottle_tag>.bottle[.<rebuild>].tar.gz` and a symlink `$CACHE/<name>--<pkg_version>.<bottle_tag>.bottle[.<rebuild>].tar.gz` pointing to it (Homebrew's symlink is `<name>--<pkg_version>` without extension as seen on disk: `jq--1.8.2 -> downloads/<hash>--jq--1.8.2.arm64_tahoe.bottle.1.tar.gz`). Use the on-disk form: symlink name `<name>--<pkg_version>`.
- Manifest: `$CACHE/downloads/<sha256 hex of the manifest URL>--<name>-<version[-rebuild]>.bottle_manifest.json` with symlink `$CACHE/<name>_bottle_manifest--<version[-rebuild]>`.
- Partial downloads use a `.incomplete` suffix.

## 3. Keg layout and receipt

- Keg: `$CELLAR/<name>/<pkg_version>/`. Contains the payload plus `INSTALL_RECEIPT.json`, `.brew/<name>.rb` (formula source snapshot, present in bottles), often `sbom.spdx.json`.
- `$PREFIX/opt/<name>` -> relative symlink `../Cellar/<name>/<version>` (also for each alias and each oldname).
- `$PREFIX/var/homebrew/linked/<name>` -> relative symlink `../../../Cellar/<name>/<version>`, present only when linked (not for keg-only).
- `$PREFIX/var/homebrew/pinned/<name>` -> relative symlink to the pinned keg. `pinned_casks/<token>` likewise for casks.
- Locks: `$PREFIX/var/homebrew/locks/<name>.formula.lock` (`flock(LOCK_EX|LOCK_NB)`), and `<token>.cask.lock` for casks. The locked path named in the error is the rack (`$CELLAR/<name>`) or `$PREFIX/Caskroom/<token>`, not the lock file. On contention Homebrew raises `OperationInProgressError` (`exceptions.rb`), printed as:

  ```text
  Error: A `brew` process has already locked <locked path>.
  Please wait for it to finish or terminate it to continue.
  ```

`INSTALL_RECEIPT.json` written after pouring a bottle (pretty JSON, 2-space
indent, key order as below; keys `built_prefix`, `padded_prefix`,
`linkage_files`, `binary_relocation_files`, `relocated_build_prefix`,
`relocated_files`, `stdlib` are omitted when null):

```json
{
  "homebrew_version": "6.0.22",
  "used_options": [],
  "unused_options": [],
  "built_as_bottle": true,
  "poured_from_bottle": true,
  "loaded_from_api": true,
  "loaded_from_internal_api": true,
  "installed_on_request": true,
  "changed_files": ["lib/pkgconfig/libjq.pc"],
  "time": 1778031361,
  "source_modified_time": 1773700688,
  "compiler": "clang",
  "aliases": [],
  "runtime_dependencies": [
    {"full_name": "oniguruma", "version": "6.9.10", "revision": 0, "bottle_rebuild": 0, "pkg_version": "6.9.10", "declared_directly": true}
  ],
  "source": {
    "spec": "stable",
    "versions": {"stable": "1.8.2", "head": null, "version_scheme": 0, "compatibility_version": null},
    "path": "/Users/me/Library/Caches/Homebrew/api/internal/packages.arm64_tahoe.jws.json",
    "tap_git_head": null,
    "tap": "homebrew/core"
  },
  "arch": "arm64",
  "built_on": {"os": "Macintosh", "os_version": "macOS 26", "cpu_family": "dunno", "xcode": "26.3", "clt": "26.3.0.0.1.1771626560", "preferred_perl": "5.34"}
}
```

Rules: `changed_files`, `linkage_files`, `binary_relocation_files`,
`source_modified_time`, `compiler`, `built_on`, `built_prefix`,
`padded_prefix` come from the manifest tab. `installed_as_dependency` is also
written (true when not requested explicitly). `runtime_dependencies` lists the
full transitive runtime closure in dependency order, `declared_directly` true
for direct deps, each with the installed pkg_version. `homebrew_version`: use
the Homebrew version fastbrew emulates (constant, currently `6.0.22`); do not
append text, Homebrew parses it as a `Version`. `time` is install time in
seconds. `source.path` is the API file path. `installed_on_request` is false
for dependencies.

## 4. Relocation

Placeholders inside bottles: `@@HOMEBREW_PREFIX@@`, `@@HOMEBREW_CELLAR@@`,
`@@HOMEBREW_REPOSITORY@@`, `@@HOMEBREW_LIBRARY@@`, `@@HOMEBREW_PERL@@` and
`@@HOMEBREW_JAVA@@`. The last two expand differently on macOS
(`extend/os/mac/keg_relocate.rb#prepare_relocation_to_locations`) than in the
generic code, and macOS wins:

- `@@HOMEBREW_PERL@@` -> `$PREFIX/opt/perl/bin/perl` when the formula is `perl`
  or lists `perl` as a directly declared runtime dependency; otherwise
  `/usr/bin/perl<tab.built_on.preferred_perl>` when that file exists, else
  `/usr/bin/perl<MacOS.preferred_perl_version>` (`5.34` on Sonoma and newer,
  `5.30` before). Only the generic (Linux) code uses
  `$PREFIX/opt/perl/bin/perl` unconditionally.
- `@@HOMEBREW_JAVA@@` -> `$PREFIX/opt/<openjdk dep name>/libexec/openjdk.jdk/Contents/Home`
  on macOS (the generic code stops at `libexec`), and only when a runtime
  dependency matches `openjdk(@n)?`.

Only `@@HOMEBREW_PREFIX@@` and `@@HOMEBREW_CELLAR@@` are ever expanded in
Mach-O install names, and only at the start of a name (`relocated_name_for`);
all six are expanded in text files.

Per cellar kind (from the API `bottle_cellar`):

1. `:any_skip_relocation` (key absent): only `skip_linkage` is set, so the Mach-O step is skipped — the text step still runs. `formula_installer.rb#pour` always calls `replace_placeholders_with_locations(tab.changed_files, skip_linkage:, ...)`, and `replace_text_in_files` runs whatever `skip_linkage` is. Skip-relocation bottles routinely need it: `ack` 3.10.0 is `:any_skip_relocation` with `changed_files: ["bin/ack"]` and a `#!@@HOMEBREW_PERL@@` shebang that must be expanded. The symlink relativization below also still runs.
2. `:any`: text replacement of placeholders in the files listed in the tab's `changed_files` (all files when the key is *missing* — an empty list means nothing to do, so the two must be told apart: scan text files, which `Keg#text_files` decides with `file`, plus `.la`/`.lai` libtool files, excluding `.brew/<name>.rb` and `Metafiles::EXTENSIONS` but always including a shebang file and `orig-prefix.txt`). Hardlinked files are rewritten once and re-linked. Then Mach-O relocation of `linkage_files` (all Mach-O files when the key is missing): replace placeholders in `LC_ID_DYLIB`, `LC_LOAD_DYLIB`/`LC_LOAD_WEAK_DYLIB`/`LC_REEXPORT_DYLIB`/`LC_LOAD_UPWARD_DYLIB` names and `LC_RPATH` paths; handle fat binaries; keep load command sizes 8-byte aligned; if the new string does not fit in the existing command and the header pad is exhausted, fall back to `install_name_tool`. The header pad ends at the first section's file offset (`MachOFile#low_fileoff`); an edit never changes the file's size, it rebuilds the load-command region in place and NUL-pads the slack. Note that modern `install_name_tool` (Xcode 26) also refuses to grow load commands past the pad ("larger updated load commands do not fit"), so that fallback only helps with layouts the in-place editor rejects; a bottle whose pad is genuinely exhausted cannot be relocated, and Homebrew fails the pour there too (ruby-macho raises `HeaderPadError` out of `change_install_name`). Re-sign every modified file: `codesign --sign - --force --preserve-metadata=entitlements,requirements,flags,runtime <file>` (parallel across files).
3. Fixed cellar (`/opt/homebrew/Cellar`): steps of 2, then if `$PREFIX` differs from the bottle's built prefix (`built_prefix` when `padded_prefix` is true, otherwise the cellar's parent), rewrite raw prefix strings inside binaries listed in `binary_relocation_files` (NUL-terminated C strings only, valid UTF-8, no control chars, at most 16384 bytes, new prefix padded with NULs or extra `/` to keep the byte length; refuse when the new prefix is longer than the old one) and re-sign. Padded prefix constant on macOS arm64: `"/opt/homebrew/.brew-padded-arm64"` left-justified with `_` to 64 bytes. Record `relocated_build_prefix` and `relocated_files` in the receipt. Otherwise Homebrew refuses to pour: `<name> was built for /opt/homebrew and can only be relocated to a prefix with a maximum length of 13 characters`.

Afterwards, absolute symlinks whose target starts with the build prefix or
its cellar are rewritten to relative links against our prefix, for every
cellar kind, whenever the build prefix differs from ours. The build cellar is
`<built_prefix>/Cellar` when the tab has a built prefix and the tag's
`default_cellar` otherwise; the built prefix is derived from trusted data (the
tag's padded-prefix constant when `padded_prefix` is true, else the cellar's
parent), never from the unauthenticated `built_prefix` annotation.

Homebrew runs the Mach-O step before the text step; the two file sets are
disjoint in practice (a Mach-O file is never in `changed_files`), so the order
does not matter.

## 5. Linking

`link` after `optlink`:

- `etc`: create directories (`mkpath`), link files.
- `bin`, `sbin`: link files directly inside; do not descend into subdirectories.
- `include`: link (subtree symlink) except `postgresql@N` -> mkpath.
- `share`: `info/*.info(.gz)` and `info/dir` are info files (link, then run `install-info`); skip `locale/locale.alias` and `icons/**/icon-theme.cache`; mkpath for anything matching `(locale|man)/<lang>[_TT][.codeset][@modifier]`, `icons/**`, `zsh*`, `fish*`, `pwsh*`, `lua/**`, `guile/**`, `postgresql@N*`, `pypy*` and for the exact paths in `Keg::SHARE_PATHS`: `aclocal`, `cps`, `doc`, `info`, `java`, `locale`, `man`, `man/man[1-8]`, `man/cat[1-8]`, `applications`, `gnome`, `gnome/help`, `icons`, `mime`, `mime/packages`, `mime-info`, `pixmaps`, `postgresql`, `sounds`; everything else: link. The prefix rules are unanchored regexes, so everything below a match is mkpath too (`share/locale/de/LC_MESSAGES` and `lib/python3.N/site-packages` are real directories in a real prefix). `javadoc`, `cmake`, `emacs`, `pkgconfig`, `bash-completion`, `elisp`, `metainfo` and `nvim` are *not* in `SHARE_PATHS` and are linked as directories.
- `lib`: skip `charset.alias`; mkpath for `cps`, `pkgconfig`, `cmake`, `dtrace`, `gdk-pixbuf*`, `ghc`, `gio*`, `lua*`, `mecab*`, `node*`, `ocaml*`, `perl5*`, `php`, `postgresql@N`, `pypy*`, `python[23].N*`, `R*`, `ruby*`; else link.
- `Frameworks`: `X.framework` and `X.framework/Versions` -> mkpath, else link.
- Skip `.DS_Store`, `.pyc/.pyo` inside `site-packages`, sources whose resolved path already is the destination, and sources resolving into another keg's `opt` path. Prune `.app` directories (never link them).
- A destination that is a symlink into another keg's directory is resolved by converting it to a real directory and linking that keg's files into it (`resolve_any_conflicts`).
- Conflict (destination exists and is not ours): abort with `Keg::ConflictError`, whose parts are joined with `\n` so the "Target" line is followed by the suggestion on its own line:

  ```text
  Error: Could not symlink <src relative to the keg>
  Target <dst>
  already exists. You may want to remove it:
    rm '<dst>'

  To force the link and overwrite all conflicting files:
    brew link --overwrite <name>

  To list all files that would be deleted:
    brew link --overwrite <name> --dry-run
  ```

  When `<dst>` resolves into another keg the middle paragraph is `is a symlink belonging to <other>. You can unlink it:` / `  brew unlink <other>` instead. `--overwrite` deletes the destination first; a destination covered by `link_overwrite_paths` is moved to `$CACHE/Backup/<path relative to the prefix>` and restored if the link fails. Roll back links created so far on failure.
- `unlink` removes every symlink in the prefix that resolves into the keg and prunes empty directories, then removes the linked record. Info files are unregistered with `install-info --delete`.

Keg-only reasons text: `:versioned_formula` -> "this is an alternate version of another formula"; `:provided_by_macos` -> "macOS already provides this software and installing another version in\nparallel can cause all kinds of trouble"; `:shadowed_by_macos` -> "macOS provides similar software and installing this software in\nparallel can cause all kinds of trouble"; a string is used verbatim. Caveat text: `<name> is keg-only, which means it was not symlinked into <prefix>,\nbecause <reason>.` followed by PATH / LDFLAGS / CPPFLAGS / PKG_CONFIG_PATH hints when `bin`, `lib`, `include`, `lib/pkgconfig` exist.

## 6. Casks

Caskroom: `$PREFIX/Caskroom/<token>/<version>/` (staged files, with symlinks to moved artifacts), `$PREFIX/Caskroom/<token>/.metadata/<version>/<timestamp>/Casks/<token>.json`, `.metadata/config.json`, `.metadata/INSTALL_RECEIPT.json`. Timestamp format `%Y%m%d%H%M%S.%L` in UTC (e.g. `20250909173901.961`). The installed cask JSON is `{"url_specs": {"only_path": ..}}` (empty object when unset) plus `"artifacts": []` when the cask has no uninstall or zap artifacts.

Cask receipt (`INSTALL_RECEIPT.json`):

```json
{"homebrew_version": "6.0.22", "loaded_from_api": true, "uninstall_flight_blocks": false, "installed_as_dependency": false, "installed_on_request": true, "time": 1757439542, "runtime_dependencies": {}, "source": {"tap": "homebrew/cask", "tap_git_head": "<sha>", "version": "1.1.3", "path": "<api file path>"}, "arch": "arm64", "uninstall_artifacts": [{"app": ["Ghostty.app"]}, {"binary": ["/Applications/Ghostty.app/Contents/MacOS/ghostty"]}, {"zap": [{"trash": [..]}]}], "built_on": {..}}
```

`uninstall_artifacts` lists artifacts in v2 JSON form with resolved paths.

`config.json`: `{"default": {"languages": [..], "appdir": "/Applications", "keyboard_layoutdir": "/Library/Keyboard Layouts", "colorpickerdir": "~/Library/ColorPickers", "prefpanedir": "~/Library/PreferencePanes", "qlplugindir": "~/Library/QuickLook", "mdimporterdir": "~/Library/Spotlight", "dictionarydir": "~/Library/Dictionaries", "fontdir": "~/Library/Fonts", "servicedir": "~/Library/Services", "input_methoddir": "~/Library/Input Methods", "internet_plugindir": "~/Library/Internet Plug-Ins", "audio_unit_plugindir": "~/Library/Audio/Plug-Ins/Components", "vst_plugindir": "~/Library/Audio/Plug-Ins/VST", "vst3_plugindir": "~/Library/Audio/Plug-Ins/VST3", "screen_saverdir": "~/Library/Screen Savers"}, "env": {..from HOMEBREW_CASK_OPTS..}, "explicit": {..from flags..}}` with `~` expanded. `binarydir` is `$PREFIX/bin`, `manpagedir` is `$PREFIX/share/man`, completions go to `$PREFIX/etc/bash_completion.d`, `$PREFIX/share/zsh/site-functions`, `$PREFIX/share/fish/vendor_completions.d`.

DMG: `hdiutil attach -plist -nobrowse -readonly -mountrandom <tmpdir> <dmg>` with stdin `qn\n` (declines EULA prompts); on failure convert with `hdiutil convert -format UDTO -o <x.cdr>` and attach that. Parse the plist `system-entities[].mount-point`. Copy contents with `ditto` (excluding `.Trashes`, `.fseventsd`, `.DS_Store`, `.background`, `.VolumeIcon.icns`, `.MobileBackups`), `chmod u+w`, then `hdiutil detach -force`. ZIP: `ditto -x -k --sequesterRsrc <zip> <dir>` (or `unzip`). Fonts are moved (not copied) into `fontdir`. App move: if the target exists and `--force` is absent, abort with `It seems there is already an App at '<target>'.`; `--adopt` accepts an identical existing app.

Quarantine: read `com.apple.quarantine` from the download (`xattr -p`); if
present, write `<flags>;<hex time>;Homebrew;<uuid>` variants exactly as
`Library/Homebrew/cask/quarantine.rb` does (set the no-translocation bit
`0x0040`? read the Ruby: it toggles bit 8 of the flags to disable
translocation) onto every staged file with `xattr -w`.

## 7. Services

Plist path in the keg: `$CELLAR/<name>/<version>/homebrew.mxcl.<name>.plist` (label `homebrew.mxcl.<name>`; `service_name_args[":macos"]` overrides both). Homebrew also writes `homebrew.<name>.service` (systemd) into the keg. Generation rules (`Library/Homebrew/service.rb#to_plist`): `Label`, `ProgramArguments` (the resolved run command, placeholders replaced), `RunAtLoad` (true unless `run_type` is `:cron`/`:interval` with `launch_only_once`... exactly: `@run_at_load` which is true for `:immediate`, false for `:interval` and `:cron`), then optionally `LaunchOnlyOnce`, `LegacyTimers`, `ExitTimeOut`, `TimeOut`, `ThrottleInterval`, `ProcessType` (capitalized), `Nice`, `StartInterval` (interval), `WorkingDirectory`, `RootDirectory`, `StandardInPath`, `StandardOutPath`, `StandardErrorPath`, `EnvironmentVariables` (with `PATH` prepended by `$PREFIX/bin:$PREFIX/sbin:/usr/bin:/bin:/usr/sbin:/sbin` when `environment_variables` includes `PATH: std_service_path_env`), `KeepAlive` (true, `{SuccessfulExit: b}`, `{Crashed: b}`, `{PathState: path}`), `Sockets`, `StartCalendarInterval` (cron fields that are not `*`), `LimitLoadToSessionType: [Aqua, Background, LoginWindow, StandardIO, System]`. XML plist, Homebrew orders keys as inserted.

Runtime: user services live in `~/Library/LaunchAgents/<label>.plist` (domain `gui/<uid>`), root services in `/Library/LaunchDaemons` (domain `system`). `start` copies the keg plist to the destination, then `launchctl enable <domain>/<label>` and `launchctl bootstrap <domain> <plist>`; `run` bootstraps the keg plist without enabling; `stop` runs `launchctl bootout <domain>/<label>` (fallback `launchctl stop <label>`) and removes the plist; `kill` sends `launchctl kill SIGTERM <domain>/<label>`. Status is read from `launchctl print <domain>/<label>` (`state = running`, `pid = N`, `last exit code = N`) with `launchctl list <label>` as fallback. `services list` prints `Name Status User File` with statuses `none`, `started`, `scheduled`, `stopped`, `error N`, `unknown`, `other`; `--json` emits `[{"name","status","user","file","exit_code"}]`.

## 8. Version comparison

Port of `Library/Homebrew/version.rb`. Tokenize with the alternation (in
this order, case-insensitive): `alpha[0-9]*|a[0-9]+`, `beta[0-9]*|b[0-9]+`,
`pre[0-9]*`, `rc[0-9]*`, `p[0-9]*`, `.post[0-9]+`, `[0-9]+`, `[a-z]+`. Compare
token by token; a missing token is the null token; numeric 0 equals null;
numeric vs non-numeric: the numeric side wins if it is greater than null,
otherwise skip it; alpha < beta < pre < rc < patch/post; composite tokens
compare their numeric suffix; string tokens compare lexically and are less
than numeric tokens. `HEAD` versions are greater than everything. Equal
version strings are equal without tokenizing. `PkgVersion` is
`<version>[_<revision>]`, compared by version then revision. A formula is
outdated when, for the latest installed keg, either the API `version_scheme`
is greater than the keg's and the pkg_versions differ, or the schemes are
equal and the API pkg_version is greater; a keg that is the current version
but is neither linked nor opt-linked nor pinned also counts as outdated.
Pinned formulae are excluded from `upgrade` but shown by `outdated`.

## 9. Output formats

`info <formula>` (not installed):

```
==> hello: stable 2.12.3 (bottled)
Program providing model for GNU coding standards and practices
https://www.gnu.org/software/hello/
Not installed
From: https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/h/hello.rb
License: GPL-3.0-or-later
==> Dependencies
Build: pkgconf ✘
Required: foo ✔, bar ✘
==> Caveats
...
```

Installed: `==> jq: stable 1.8.2 (bottled)`, then desc, homepage, `Installed`,
`/opt/homebrew/Cellar/jq/1.8.2 (20 files, 1.2MB) *` (asterisk when linked),
`  Poured from bottle using the formulae.brew.sh API on 2026-05-06 at 10:36:12`,
`From: ...`, `License: ...`, `==> Dependencies`, `==> Options` when any,
`==> Caveats`, `==> Analytics` (skip unless requested). Outdated formulae show
`stable 1.8.1 → 1.8.2` in the title. `==> Installed Versions` header only with
`--verbose`. Deprecated/disabled lines follow the homepage. The formula path
in `From:` is `Formula/<first letter>/<name>.rb` (`Formula/lib/` for names
starting with `lib`? No: Homebrew uses sharded dirs: names starting with
`lib` go to `Formula/lib/`, otherwise `Formula/<first char>/`).

`list`: names in columns like `ls -C` on a TTY, one per line otherwise.
`list --versions`: `name version [version ...]`. `outdated`: names; with
`--verbose`: `name (installed) < current [pinned at x]`. Install progress:
`==> Fetching hello`, `==> Downloading https://ghcr.io/v2/homebrew/core/hello/manifests/2.12.3-1`, `Already downloaded: <path>` or a progress bar, `==> Pouring hello--2.12.3.arm64_tahoe.bottle.1.tar.gz`, `==> Caveats` block, `🍺  /opt/homebrew/Cellar/hello/2.12.3: 8 files, 186KB`. Dependencies print `==> Installing dependencies for jq: oniguruma` then `==> Installing jq dependency: oniguruma`, and after everything `==> Installing jq`. Uninstall prints `Uninstalling /opt/homebrew/Cellar/hello/2.12.3... (8 files, 186KB)`. Dependents block: `Error: Refusing to uninstall /opt/homebrew/Cellar/oniguruma/6.9.10\nbecause it is required by jq, which is currently installed.\nYou can override this and force removal with:\n  brew uninstall --ignore-dependencies oniguruma`.

## 10. Environment variables honored

`HOMEBREW_PREFIX`, `HOMEBREW_CELLAR`, `HOMEBREW_REPOSITORY`,
`HOMEBREW_LIBRARY`, `HOMEBREW_CACHE`, `HOMEBREW_LOGS`, `HOMEBREW_TEMP`,
`HOMEBREW_API_DOMAIN`, `HOMEBREW_BOTTLE_DOMAIN`, `HOMEBREW_ARTIFACT_DOMAIN`,
`HOMEBREW_GITHUB_PACKAGES_TOKEN`, `HOMEBREW_GITHUB_PACKAGES_USER`,
`HOMEBREW_NO_AUTO_UPDATE`, `HOMEBREW_AUTO_UPDATE_SECS`,
`HOMEBREW_API_AUTO_UPDATE_SECS`, `HOMEBREW_NO_INSTALL_CLEANUP`,
`HOMEBREW_NO_INSTALL_UPGRADE`, `HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK`,
`HOMEBREW_NO_EMOJI`, `HOMEBREW_NO_COLOR`, `HOMEBREW_COLOR`, `NO_COLOR`,
`HOMEBREW_NO_ENV_HINTS`, `HOMEBREW_VERBOSE`, `HOMEBREW_DEBUG`,
`HOMEBREW_CASK_OPTS`, `HOMEBREW_DOWNLOAD_CONCURRENCY`,
`HOMEBREW_CLEANUP_MAX_AGE_DAYS` (default 120), `HOMEBREW_CURL_RETRIES`,
`HOMEBREW_NO_ANALYTICS` (fastbrew never sends analytics), `HOMEBREW_BAT`
(ignored), `HOMEBREW_INSTALL_BADGE`.

fastbrew-specific: `FASTBREW_BREW` (path to the Ruby `brew` for delegation),
`FASTBREW_REQUIRE_SANDBOX=1` (refuse the standard prefixes),
`FASTBREW_NO_DELEGATE=1` (fail instead of delegating), `FASTBREW_LOG`
(tracing filter).
