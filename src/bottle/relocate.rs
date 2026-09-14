//! Pour-time relocation (`docs/COMPAT.md` 4).
//!
//! `relocate_keg` applies, in order: placeholder text replacement in
//! `changed_files` (or a scan of text and libtool files when the tab has no
//! list), Mach-O placeholder rewriting in `linkage_files` (or all Mach-O
//! files) followed by re-signing, build-prefix rewriting in
//! `binary_relocation_files` when the prefix differs from the built prefix,
//! and symlink relativization. Returns what was changed so the receipt can
//! record `relocated_build_prefix`/`relocated_files`.
//!
//! Ported from `keg_relocate.rb` (`replace_text_in_files`,
//! `relocate_build_prefix`, `replace_prefix_preserving_length`,
//! `relativize_prefix_symlinks!`), `extend/os/mac/keg_relocate.rb`
//! (`relocate_dynamic_linkage`, `relocated_name_for`,
//! `prepare_relocation_to_locations`) and `formula_installer.rb#pour`, which
//! decides which of those steps a given cellar kind needs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::formula::BottleCellar;

use super::{BottleTab, codesign, macho};

pub const PREFIX_PLACEHOLDER: &str = "@@HOMEBREW_PREFIX@@";
pub const CELLAR_PLACEHOLDER: &str = "@@HOMEBREW_CELLAR@@";
pub const REPOSITORY_PLACEHOLDER: &str = "@@HOMEBREW_REPOSITORY@@";
pub const LIBRARY_PLACEHOLDER: &str = "@@HOMEBREW_LIBRARY@@";
pub const PERL_PLACEHOLDER: &str = "@@HOMEBREW_PERL@@";
pub const JAVA_PLACEHOLDER: &str = "@@HOMEBREW_JAVA@@";

/// Longest chunk between NUL bytes that may still be a C string
/// (`Keg::MAX_C_STRING_BYTESIZE`).
const MAX_C_STRING_BYTESIZE: usize = 16_384;

/// How much of a file is inspected for NUL bytes when deciding it is text.
const TEXT_SNIFF_BYTES: usize = 8 * 1024;

/// Libtool archives are always rewritten, whatever `file` thinks of them
/// (`Keg::LIBTOOL_EXTENSIONS`).
const LIBTOOL_EXTENSIONS: [&str; 2] = ["la", "lai"];

/// `Metafiles::EXTENSIONS`: documentation that never carries linkage.
const METAFILE_EXTENSIONS: [&str; 18] = [
    "adoc",
    "asc",
    "asciidoc",
    "creole",
    "html",
    "markdown",
    "md",
    "mdown",
    "mediawiki",
    "mkdn",
    "org",
    "pod",
    "rdoc",
    "rst",
    "rtf",
    "textile",
    "txt",
    "wiki",
];

#[derive(Debug, Default)]
pub struct RelocationReport {
    pub text_files_changed: Vec<String>,
    pub macho_files_changed: Vec<String>,
    pub relocated_build_prefix: Option<String>,
    pub relocated_files: Vec<String>,
}

pub struct RelocateArgs<'a> {
    pub keg_path: &'a Path,
    pub cellar_kind: &'a BottleCellar,
    pub tab: &'a BottleTab,
    /// Name of an `openjdk*` runtime dependency, for `@@HOMEBREW_JAVA@@`.
    pub openjdk_dep: Option<&'a str>,
}

/// Everything `@@HOMEBREW_*@@` expands to for one keg
/// (`prepare_relocation_to_locations`, with the macOS overrides).
#[derive(Debug, Clone)]
pub struct Replacements {
    pairs: Vec<(String, String)>,
}

impl Replacements {
    /// `tab` supplies `built_on.preferred_perl`; `name` the keg's formula name.
    pub fn new(
        cfg: &Config,
        tab: Option<&BottleTab>,
        openjdk_dep: Option<&str>,
        name: Option<&str>,
    ) -> Replacements {
        let prefix = cfg.prefix.to_string_lossy().into_owned();
        let mut pairs = vec![
            (PREFIX_PLACEHOLDER.to_string(), prefix.clone()),
            (
                CELLAR_PLACEHOLDER.to_string(),
                cfg.cellar.to_string_lossy().into_owned(),
            ),
            (
                REPOSITORY_PLACEHOLDER.to_string(),
                cfg.repository.to_string_lossy().into_owned(),
            ),
            (
                LIBRARY_PLACEHOLDER.to_string(),
                cfg.library.to_string_lossy().into_owned(),
            ),
            (PERL_PLACEHOLDER.to_string(), perl_path(cfg, tab, name)),
        ];
        if let Some(openjdk) = openjdk_dep {
            // macOS expands to the JDK home inside `libexec`
            // (`extend/os/mac/keg_relocate.rb#prepare_relocation_to_locations`).
            pairs.push((
                JAVA_PLACEHOLDER.to_string(),
                format!("{prefix}/opt/{openjdk}/libexec/openjdk.jdk/Contents/Home"),
            ));
        }
        // `Relocation#replace_text!` applies the longest key first so that no
        // placeholder is a prefix of another's replacement.
        pairs.sort_by_key(|(key, _)| std::cmp::Reverse(key.len()));
        Replacements { pairs }
    }

    /// Replacement for a Mach-O install name (`relocated_name_for`): only the
    /// cellar and prefix placeholders, and only at the start of the name.
    fn install_name(&self, old: &str) -> Option<String> {
        for placeholder in [CELLAR_PLACEHOLDER, PREFIX_PLACEHOLDER] {
            if let Some(rest) = old.strip_prefix(placeholder) {
                let new = self.value(placeholder)?;
                return Some(format!("{new}{rest}"));
            }
        }
        None
    }

    fn value(&self, placeholder: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == placeholder)
            .map(|(_, v)| v.as_str())
    }

    /// Replace every placeholder in `text`; returns whether anything changed.
    pub fn apply(&self, text: &mut Vec<u8>) -> bool {
        let mut changed = false;
        for (from, to) in &self.pairs {
            changed |= replace_all(text, from.as_bytes(), to.as_bytes());
        }
        changed
    }
}

/// `/usr/bin/perlX.Y` unless the keg is `perl` or declares it directly
/// (`extend/os/mac/keg_relocate.rb`).
fn perl_path(cfg: &Config, tab: Option<&BottleTab>, name: Option<&str>) -> String {
    let brewed = name == Some("perl")
        || tab.is_some_and(|t| {
            t.runtime_dependencies
                .iter()
                .any(|d| d.full_name == "perl" && d.declared_directly)
        });
    if brewed {
        return format!("{}/opt/perl/bin/perl", cfg.prefix.display());
    }
    if let Some(version) = tab
        .and_then(|t| t.built_on.as_ref())
        .and_then(|b| b.get("preferred_perl"))
        .and_then(|v| v.as_str())
        .filter(|v| {
            let mut parts = v.split('.');
            matches!((parts.next(), parts.next(), parts.next()), (Some(a), Some(b), None)
                if !a.is_empty() && a.bytes().all(|c| c.is_ascii_digit())
                    && !b.is_empty() && b.bytes().all(|c| c.is_ascii_digit()))
        })
    {
        let path = format!("/usr/bin/perl{version}");
        if Path::new(&path).exists() {
            return path;
        }
    }
    format!("/usr/bin/perl{}", preferred_perl_version())
}

/// `MacOS.preferred_perl_version` (`os/mac.rb`).
fn preferred_perl_version() -> &'static str {
    let sonoma = 14;
    match crate::platform::Host::detect().macos {
        Some(v) if v.major >= sonoma => "5.34",
        Some(_) => "5.30",
        None => "5.34",
    }
}

/// Replace placeholders in one text buffer; returns whether anything changed.
///
/// Convenience wrapper over [`Replacements`] for callers with no bottle tab;
/// `relocate_keg` builds the tab-aware replacements itself.
pub fn replace_placeholders(cfg: &Config, text: &mut Vec<u8>, openjdk_dep: Option<&str>) -> bool {
    Replacements::new(cfg, None, openjdk_dep, None).apply(text)
}

/// Apply every relocation step this bottle's cellar kind calls for.
pub fn relocate_keg(cfg: &Config, args: RelocateArgs<'_>) -> Result<RelocationReport> {
    let keg = args.keg_path;
    let name = keg
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());
    let repl = Replacements::new(cfg, Some(args.tab), args.openjdk_dep, name);
    let mut report = RelocationReport::default();

    let skip_linkage = matches!(args.cellar_kind, BottleCellar::AnySkipRelocation);

    // 1. Text placeholders in the recorded files, or in every text and libtool
    //    file when the tab carries no list.
    let text_targets = match &args.tab.changed_files {
        Some(list) => keg_files(keg, list),
        None => scan_text_files(keg, name),
    };
    report.text_files_changed = replace_text_in_files(keg, &text_targets, &repl)?;

    // 2. Mach-O placeholders, then re-sign every file we touched.
    if !skip_linkage {
        let linkage_targets = match &args.tab.linkage_files {
            Some(list) => keg_files(keg, list),
            None => mach_o_files(keg),
        };
        report.macho_files_changed = relocate_dynamic_linkage(keg, &linkage_targets, &repl)?;
    }

    // 3. The build prefix baked into binaries of a fixed-cellar bottle.
    let (build_prefix, build_cellar) = built_locations(args.cellar_kind, args.tab);
    if let (Some(build_prefix), BottleCellar::Fixed(_)) =
        (build_prefix.as_deref(), args.cellar_kind)
    {
        let ours = cfg.prefix.to_string_lossy().into_owned();
        let our_cellar = cfg.cellar.to_string_lossy().into_owned();
        let same = build_prefix == ours && build_cellar.as_deref().is_some_and(|c| c == our_cellar);
        if !same {
            if build_prefix.len() < ours.len() {
                return Err(Error::user(format!(
                    "{} was built for {build_prefix} and can only be relocated to a prefix with a maximum length of {} characters",
                    name.unwrap_or("this formula"),
                    build_prefix.len()
                )));
            }
            report.relocated_files = relocate_build_prefix(
                keg,
                build_prefix,
                &ours,
                args.tab.binary_relocation_files.as_deref(),
            )?;
            report.relocated_build_prefix = Some(build_prefix.to_string());
        }
    }

    // 4. Absolute symlinks into the build prefix become relative links into
    //    ours (`relativize_prefix_symlinks!`).
    if let (Some(build_prefix), Some(build_cellar)) = (&build_prefix, &build_cellar)
        && (Path::new(build_prefix) != cfg.prefix || Path::new(build_cellar) != cfg.cellar)
    {
        relativize_prefix_symlinks(cfg, keg, build_prefix, build_cellar)?;
    }

    Ok(report)
}

/// `(build prefix, build cellar)` the bottle was made for, as
/// `formula_installer.rb#pour` and `Utils::Bottles.load_tab` derive them.
fn built_locations(
    cellar_kind: &BottleCellar,
    tab: &BottleTab,
) -> (Option<String>, Option<String>) {
    // A padded build prefix is not taken from the (unauthenticated) annotation
    // but from the tag's constant, exactly as `load_tab` does.
    if tab.padded_prefix == Some(true)
        && let Some(padded) = crate::platform::Host::detect().bottle_tag().padded_prefix()
    {
        let cellar = format!("{padded}/Cellar");
        return (Some(padded), Some(cellar));
    }
    let cellar = match cellar_kind {
        BottleCellar::Fixed(c) => c.clone(),
        // Relocatable bottles were still built somewhere: the tag's default.
        _ => crate::platform::Host::detect()
            .bottle_tag()
            .default_cellar()
            .to_string(),
    };
    let prefix = Path::new(&cellar)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| tab.built_prefix.clone());
    (prefix, Some(cellar))
}

// ------------------------------------------------------------------ text

/// Rewrite placeholders in `files`, once per inode, re-linking hard links
/// afterwards. Returns the changed paths relative to the keg.
fn replace_text_in_files(
    keg: &Path,
    files: &[PathBuf],
    repl: &Replacements,
) -> Result<Vec<String>> {
    let mut changed = Vec::new();
    for group in group_by_inode(files) {
        let first = &group[0];
        let mut data = match std::fs::read(first) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if !repl.apply(&mut data) {
            continue;
        }
        super::with_writable(first, || atomic_write_preserving_mode(first, &data))?;
        for other in &group[1..] {
            let _ = std::fs::remove_file(other);
            let _ = std::fs::hard_link(first, other);
        }
        for file in &group {
            changed.push(relative_to(keg, file));
        }
    }
    changed.sort();
    Ok(changed)
}

/// Every text or libtool file in the keg (`Keg#text_files | #libtool_files`),
/// approximating `file`'s verdict with a NUL-byte sniff.
fn scan_text_files(keg: &Path, name: Option<&str>) -> Vec<PathBuf> {
    let brew_formula = name.map(|n| keg.join(".brew").join(format!("{n}.rb")));
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(keg).follow_links(false) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let basename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if LIBTOOL_EXTENSIONS.contains(&extension.as_str()) {
            out.push(path.to_path_buf());
            continue;
        }
        // A python virtualenv's `orig-prefix.txt` is always rewritten even
        // though its extension is a metafile one.
        if basename != "orig-prefix.txt" {
            if brew_formula.as_deref() == Some(path) {
                continue;
            }
            if METAFILE_EXTENSIONS.contains(&extension.as_str()) {
                continue;
            }
        }
        if looks_like_text(path) {
            out.push(path.to_path_buf());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A shebang makes a file text whatever its bytes look like
/// (`Utils::Path.text_executable?`); otherwise no NUL byte in the first 8 KiB.
fn looks_like_text(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; TEXT_SNIFF_BYTES];
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return false,
        }
    }
    let head = &head[..filled];
    if is_text_executable(head) {
        return true;
    }
    !head.contains(&0)
}

/// `/\A#!\s*\S+/` over the first bytes of the file.
fn is_text_executable(head: &[u8]) -> bool {
    let Some(rest) = head.strip_prefix(b"#!") else {
        return false;
    };
    let rest = rest
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map(|i| &rest[i..]);
    rest.is_some_and(|r| !r.is_empty())
}

// ---------------------------------------------------------------- Mach-O

/// Rewrite placeholdered dylib IDs, install names and rpaths, then re-sign.
fn relocate_dynamic_linkage(
    keg: &Path,
    files: &[PathBuf],
    repl: &Replacements,
) -> Result<Vec<String>> {
    let map = |old: &str| repl.install_name(old);
    let outcomes: Vec<(PathBuf, Result<bool>)> = files
        .par_iter()
        .map(|file| {
            let modified =
                super::with_writable(file, || match macho::rewrite_install_names(file, &map) {
                    Ok(modified) => Ok(modified),
                    // The load commands no longer fit before the first section.
                    Err(macho::MachOError::NoHeaderPad { .. }) => {
                        install_name_tool_fallback(file, &map)
                    }
                    Err(e) => Err(e.into()),
                });
            (file.clone(), modified)
        })
        .collect();

    // Sign what did change before reporting a failure, so the keg is never
    // left holding a modified file with a stale signature.
    let changed: Vec<&Path> = outcomes
        .iter()
        .filter(|(_, r)| matches!(r, Ok(true)))
        .map(|(f, _)| f.as_path())
        .collect();
    codesign::codesign_files(&changed)?;
    let mut names: Vec<String> = changed.iter().map(|f| relative_to(keg, f)).collect();
    names.sort();

    // Homebrew re-raises a `MachO::MachOError` out of `change_install_name`,
    // failing the pour rather than installing half-relocated linkage.
    if let Some((file, Err(e))) = outcomes.iter().find(|(_, r)| r.is_err()) {
        return Err(Error::user(format!(
            "Failed changing install names in {}\n{e}",
            file.display()
        )));
    }
    Ok(names)
}

/// When the load commands no longer fit in the header pad, let
/// `install_name_tool` rewrite the file (it relays the whole binary out).
fn install_name_tool_fallback(file: &Path, map: &dyn Fn(&str) -> Option<String>) -> Result<bool> {
    let info = macho::read_info(file)?;
    let mut args: Vec<String> = Vec::new();
    if let Some(id) = info.dylib_id.as_deref()
        && let Some(new) = map(id)
        && new != id
    {
        args.push("-id".into());
        args.push(new);
    }
    for old in &info.linked_libraries {
        if let Some(new) = map(old)
            && &new != old
        {
            args.push("-change".into());
            args.push(old.clone());
            args.push(new);
        }
    }
    for old in &info.rpaths {
        if let Some(new) = map(old)
            && &new != old
        {
            args.push("-rpath".into());
            args.push(old.clone());
            args.push(new);
        }
    }
    if args.is_empty() {
        return Ok(false);
    }
    let out = std::process::Command::new("/usr/bin/install_name_tool")
        .args(&args)
        .arg(file)
        .output()
        .map_err(|e| {
            Error::Other(anyhow::Error::new(e).context("running /usr/bin/install_name_tool"))
        })?;
    if !out.status.success() {
        return Err(Error::user(format!(
            "install_name_tool failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(true)
}

/// Every Mach-O executable, dylib or bundle in the keg, one name per inode
/// (`Keg#mach_o_files`).
fn mach_o_files(keg: &Path) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(keg).follow_links(false) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !macho::is_macho(path) {
            continue;
        }
        if let Ok(md) = entry.metadata() {
            use std::os::unix::fs::MetadataExt;
            if !seen.insert((md.dev(), md.ino())) {
                continue;
            }
        }
        out.push(path.to_path_buf());
    }
    out.sort();
    out
}

// --------------------------------------------------------- build prefix

/// Patch raw NUL-terminated build-prefix strings in binaries, keeping every
/// string's byte length (`Keg#relocate_build_prefix`).
pub fn relocate_build_prefix(
    keg: &Path,
    old_prefix: &str,
    new_prefix: &str,
    files: Option<&[String]>,
) -> Result<Vec<String>> {
    if new_prefix.len() > old_prefix.len() {
        return Err(Error::user(format!(
            "Cannot relocate build prefix {old_prefix} to longer prefix {new_prefix}"
        )));
    }
    let candidates = match files {
        Some(list) => {
            let mut candidates = keg_files(keg, list);
            // Bottle metadata records one name per inode; the other names of a
            // hard-linked file only turn up in a walk, so do one when needed.
            let hardlinked: BTreeSet<u64> = candidates
                .iter()
                .filter_map(|f| std::fs::metadata(f).ok())
                .filter(|m| {
                    use std::os::unix::fs::MetadataExt;
                    m.nlink() > 1
                })
                .map(|m| {
                    use std::os::unix::fs::MetadataExt;
                    m.ino()
                })
                .collect();
            if !hardlinked.is_empty() {
                for entry in walkdir::WalkDir::new(keg).follow_links(false) {
                    let Ok(entry) = entry else { continue };
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    if let Ok(md) = entry.metadata() {
                        use std::os::unix::fs::MetadataExt;
                        if hardlinked.contains(&md.ino()) {
                            candidates.push(entry.path().to_path_buf());
                        }
                    }
                }
            }
            candidates.sort();
            candidates.dedup();
            candidates
        }
        None => files_containing(keg, old_prefix),
    };

    let mut patched_groups: Vec<Vec<PathBuf>> = Vec::new();
    for group in group_by_inode(&candidates) {
        let file = &group[0];
        let Ok(data) = std::fs::read(file) else {
            continue;
        };
        // Only binaries need NUL padding; sharballs break when patched.
        if !data.contains(&0) || is_text_executable(&data) {
            continue;
        }
        let Some(patched) = patch_prefix_strings(&data, old_prefix, new_prefix) else {
            continue;
        };
        if patched.len() != data.len() {
            return Err(Error::user(format!(
                "Patching failed!  Original and patched binary sizes do not match.\nOriginal size: {}\nPatched size: {}",
                data.len(),
                patched.len()
            )));
        }
        super::with_writable(file, || atomic_write_preserving_mode(file, &patched))?;
        patched_groups.push(group);
    }

    let firsts: Vec<&Path> = patched_groups.iter().map(|g| g[0].as_path()).collect();
    codesign::codesign_files(&firsts)?;

    let mut out = Vec::new();
    for group in &patched_groups {
        for other in &group[1..] {
            let _ = std::fs::remove_file(other);
            let _ = std::fs::hard_link(&group[0], other);
        }
        for file in group {
            out.push(relative_to(keg, file));
        }
    }
    out.sort();
    Ok(out)
}

/// Replace `old_prefix` inside every NUL-delimited chunk that could be a C
/// string, or `None` when the file carries none.
fn patch_prefix_strings(data: &[u8], old_prefix: &str, new_prefix: &str) -> Option<Vec<u8>> {
    let mut changed = false;
    let mut out = Vec::with_capacity(data.len());
    for (i, chunk) in data.split(|b| *b == 0).enumerate() {
        if i > 0 {
            out.push(0);
        }
        if is_c_string_candidate(chunk, old_prefix) {
            changed = true;
            out.extend_from_slice(&replace_prefix_preserving_length(
                chunk,
                old_prefix.as_bytes(),
                new_prefix.as_bytes(),
            ));
        } else {
            out.extend_from_slice(chunk);
        }
    }
    changed.then_some(out)
}

fn is_c_string_candidate(chunk: &[u8], old_prefix: &str) -> bool {
    if chunk.len() > MAX_C_STRING_BYTESIZE {
        return false;
    }
    if !contains(chunk, old_prefix.as_bytes()) {
        return false;
    }
    let Ok(text) = std::str::from_utf8(chunk) else {
        return false;
    };
    // `C_STRING_REGEX`: no control characters other than tab, newline, return.
    text.chars()
        .all(|c| matches!(c, '\t' | '\n' | '\r') || !c.is_control())
}

/// `Keg.replace_prefix_preserving_length` without the ELF suffix-offset case,
/// which never applies on macOS: replace every occurrence and NUL-pad the tail
/// back to the original length.
pub fn replace_prefix_preserving_length(string: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(string.len());
    let mut i = 0;
    while i < string.len() {
        if string[i..].starts_with(old) {
            out.extend_from_slice(new);
            i += old.len();
        } else {
            out.push(string[i]);
            i += 1;
        }
    }
    out.resize(string.len(), 0);
    out
}

/// Files under the keg whose bytes contain `needle` (`files_matching_by_inode`).
fn files_containing(keg: &Path, needle: &str) -> Vec<PathBuf> {
    let entries: Vec<PathBuf> = walkdir::WalkDir::new(keg)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .collect();
    let mut out: Vec<PathBuf> = entries
        .par_iter()
        .filter(|path| {
            std::fs::read(path)
                .map(|d| contains(&d, needle.as_bytes()))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    out.sort();
    out
}

// --------------------------------------------------------------- symlinks

/// Absolute symlinks into the build prefix or its cellar become relative links
/// into ours (`Keg#relativize_prefix_symlinks!`).
pub fn relativize_prefix_symlinks(
    cfg: &Config,
    keg: &Path,
    build_prefix: &str,
    build_cellar: &str,
) -> Result<()> {
    for entry in walkdir::WalkDir::new(keg).follow_links(false) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_symlink() {
            continue;
        }
        let file = entry.path();
        let Ok(link) = std::fs::read_link(file) else {
            continue;
        };
        if !link.is_absolute() {
            continue;
        }
        let link = link.to_string_lossy().into_owned();
        let target = if let Some(rest) = link.strip_prefix(&format!("{build_cellar}/")) {
            cfg.cellar.join(rest)
        } else if let Some(rest) = link.strip_prefix(&format!("{build_prefix}/")) {
            cfg.prefix.join(rest)
        } else {
            continue;
        };
        let parent = file.parent().unwrap_or_else(|| Path::new("/"));
        let new = crate::keg::relative_path(&target, parent);
        std::fs::remove_file(file)?;
        std::os::unix::fs::symlink(new, file)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- helpers

/// Regular files the keg-relative paths refer to, ignoring anything that would
/// escape the keg (`Keg#keg_files`).
fn keg_files(keg: &Path, relative_paths: &[String]) -> Vec<PathBuf> {
    let Ok(keg_real) = std::fs::canonicalize(keg) else {
        return vec![];
    };
    relative_paths
        .iter()
        .filter_map(|rel| {
            let file = normalize(&keg.join(rel));
            if !file.starts_with(keg) {
                return None;
            }
            let md = std::fs::symlink_metadata(&file).ok()?;
            if md.file_type().is_symlink() || !md.is_file() {
                return None;
            }
            let real = std::fs::canonicalize(&file).ok()?;
            real.starts_with(&keg_real).then_some(file)
        })
        .collect()
}

/// `Pathname#cleanpath`: resolve `.` and `..` lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn relative_to(keg: &Path, file: &Path) -> String {
    file.strip_prefix(keg)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned()
}

/// Group paths sharing an inode so a hard-linked file is rewritten once.
fn group_by_inode(files: &[PathBuf]) -> Vec<Vec<PathBuf>> {
    use std::os::unix::fs::MetadataExt;
    let mut groups: BTreeMap<(u64, u64), Vec<PathBuf>> = BTreeMap::new();
    let mut ungrouped = Vec::new();
    for file in files {
        match std::fs::metadata(file) {
            Ok(md) => groups
                .entry((md.dev(), md.ino()))
                .or_default()
                .push(file.clone()),
            Err(_) => ungrouped.push(vec![file.clone()]),
        }
    }
    groups.into_values().chain(ungrouped).collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn replace_all(buf: &mut Vec<u8>, from: &[u8], to: &[u8]) -> bool {
    if from.is_empty() || buf.len() < from.len() {
        return false;
    }
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    let mut changed = false;
    while i < buf.len() {
        if buf[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
            changed = true;
        } else {
            out.push(buf[i]);
            i += 1;
        }
    }
    if changed {
        *buf = out;
    }
    changed
}

/// `Pathname#atomic_write`: write through a temporary file in the same
/// directory and restore the original mode afterwards.
fn atomic_write_preserving_mode(path: &Path, data: &[u8]) -> Result<()> {
    let mode = std::fs::metadata(path).ok().map(|m| m.permissions());
    crate::keg::atomic_write(path, data)?;
    if let Some(mode) = mode {
        let _ = std::fs::set_permissions(path, mode);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn keg_with(files: &[(&str, &[u8])]) -> (tempfile::TempDir, Config, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let keg = cfg.cellar.join("demo/1.0");
        for (rel, body) in files {
            let path = keg.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
        }
        (tmp, cfg, keg)
    }

    #[test]
    fn replaces_placeholders_in_listed_files() {
        let (_tmp, cfg, keg) = keg_with(&[
            (
                "lib/pkgconfig/demo.pc",
                b"prefix=@@HOMEBREW_CELLAR@@/demo/1.0\nlibdir=@@HOMEBREW_PREFIX@@/lib\n",
            ),
            ("share/untouched", b"@@HOMEBREW_PREFIX@@/nope\n"),
        ]);
        let tab = BottleTab {
            changed_files: Some(vec!["lib/pkgconfig/demo.pc".into()]),
            ..Default::default()
        };
        let report = relocate_keg(
            &cfg,
            RelocateArgs {
                keg_path: &keg,
                cellar_kind: &BottleCellar::Any,
                tab: &tab,
                openjdk_dep: None,
            },
        )
        .unwrap();
        assert_eq!(report.text_files_changed, vec!["lib/pkgconfig/demo.pc"]);
        let pc = std::fs::read_to_string(keg.join("lib/pkgconfig/demo.pc")).unwrap();
        assert!(
            pc.contains(&format!("prefix={}/demo/1.0", cfg.cellar.display())),
            "{pc}"
        );
        assert!(
            pc.contains(&format!("libdir={}/lib", cfg.prefix.display())),
            "{pc}"
        );
        // A file outside `changed_files` is untouched.
        assert!(
            std::fs::read_to_string(keg.join("share/untouched"))
                .unwrap()
                .contains("@@HOMEBREW_PREFIX@@")
        );
    }

    #[test]
    fn scans_text_and_libtool_files_without_a_list() {
        let (_tmp, cfg, keg) = keg_with(&[
            ("bin/script", b"#!/bin/sh\nexec @@HOMEBREW_PREFIX@@/bin/x\n"),
            ("lib/libdemo.la", b"libdir='@@HOMEBREW_PREFIX@@/lib'\n"),
            ("README.md", b"see @@HOMEBREW_PREFIX@@\n"),
            ("lib/blob.bin", b"\x00\x01@@HOMEBREW_PREFIX@@\x00"),
        ]);
        let tab = BottleTab::default();
        let report = relocate_keg(
            &cfg,
            RelocateArgs {
                keg_path: &keg,
                cellar_kind: &BottleCellar::Any,
                tab: &tab,
                openjdk_dep: None,
            },
        )
        .unwrap();
        assert_eq!(
            report.text_files_changed,
            vec!["bin/script".to_string(), "lib/libdemo.la".to_string()]
        );
        // Metafile extensions and binaries are skipped by the scan.
        assert!(
            std::fs::read_to_string(keg.join("README.md"))
                .unwrap()
                .contains("@@HOMEBREW_PREFIX@@")
        );
        assert!(contains(
            &std::fs::read(keg.join("lib/blob.bin")).unwrap(),
            b"@@HOMEBREW_PREFIX@@"
        ));
    }

    #[test]
    fn rewrites_hardlinked_files_once_and_relinks_them() {
        let (_tmp, cfg, keg) = keg_with(&[("a/one", b"@@HOMEBREW_PREFIX@@/x\n")]);
        std::fs::create_dir_all(keg.join("b")).unwrap();
        std::fs::hard_link(keg.join("a/one"), keg.join("b/two")).unwrap();
        let tab = BottleTab {
            changed_files: Some(vec!["a/one".into(), "b/two".into()]),
            ..Default::default()
        };
        let report = relocate_keg(
            &cfg,
            RelocateArgs {
                keg_path: &keg,
                cellar_kind: &BottleCellar::Any,
                tab: &tab,
                openjdk_dep: None,
            },
        )
        .unwrap();
        assert_eq!(
            report.text_files_changed,
            vec!["a/one".to_string(), "b/two".to_string()]
        );
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(keg.join("a/one")).unwrap().ino(),
            std::fs::metadata(keg.join("b/two")).unwrap().ino(),
            "hard link was not restored"
        );
        assert_eq!(
            std::fs::read_to_string(keg.join("b/two")).unwrap(),
            format!("{}/x\n", cfg.prefix.display())
        );
    }

    #[test]
    fn preserves_the_mode_of_rewritten_files() {
        let (_tmp, cfg, keg) = keg_with(&[("bin/run", b"#!/bin/sh\n@@HOMEBREW_PREFIX@@/bin/x\n")]);
        std::fs::set_permissions(keg.join("bin/run"), std::fs::Permissions::from_mode(0o555))
            .unwrap();
        let tab = BottleTab {
            changed_files: Some(vec!["bin/run".into()]),
            ..Default::default()
        };
        relocate_keg(
            &cfg,
            RelocateArgs {
                keg_path: &keg,
                cellar_kind: &BottleCellar::Any,
                tab: &tab,
                openjdk_dep: None,
            },
        )
        .unwrap();
        let mode = std::fs::metadata(keg.join("bin/run"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o555);
    }

    #[test]
    fn keg_files_ignores_paths_escaping_the_keg() {
        let (_tmp, _cfg, keg) = keg_with(&[("in", b"x")]);
        std::fs::write(keg.parent().unwrap().join("outside"), b"y").unwrap();
        let found = keg_files(
            &keg,
            &[
                "in".into(),
                "../outside".into(),
                "/etc/hosts".into(),
                "missing".into(),
            ],
        );
        assert_eq!(found, vec![keg.join("in")]);
    }

    #[test]
    fn c_string_padding_keeps_the_byte_length() {
        let old = b"/opt/homebrew";
        let new = b"/tmp/p";
        let s = b"/opt/homebrew/lib:/opt/homebrew/share";
        let out = replace_prefix_preserving_length(s, old, new);
        assert_eq!(out.len(), s.len());
        assert!(out.starts_with(b"/tmp/p/lib:/tmp/p/share"));
        assert!(
            out[b"/tmp/p/lib:/tmp/p/share".len()..]
                .iter()
                .all(|b| *b == 0)
        );
    }

    #[test]
    fn build_prefix_relocation_patches_only_c_strings() {
        let (_tmp, _cfg, keg) = keg_with(&[]);
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        // A NUL-delimited "binary": a plain C string, a string with a control
        // character (must not be patched) and an over-long chunk.
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(b"\x7fELF-ish header\x00");
        body.extend_from_slice(b"/opt/homebrew/lib/libz.dylib\x00");
        body.extend_from_slice(b"/opt/homebrew/\x07bell\x00");
        let long = format!("{}/opt/homebrew", "x".repeat(MAX_C_STRING_BYTESIZE));
        body.extend_from_slice(long.as_bytes());
        body.push(0);
        let binary = keg.join("bin/demo");
        std::fs::write(&binary, &body).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

        let changed =
            relocate_build_prefix(&keg, "/opt/homebrew", "/tmp/p", Some(&["bin/demo".into()]))
                .unwrap();
        assert_eq!(changed, vec!["bin/demo"]);
        let out = std::fs::read(&binary).unwrap();
        assert_eq!(out.len(), body.len(), "size must not change");
        assert!(contains(&out, b"/tmp/p/lib/libz.dylib\x00"));
        // The chunk with a control byte and the over-long chunk keep the old prefix.
        assert!(contains(&out, b"/opt/homebrew/\x07bell"));
        assert!(contains(&out, b"x/opt/homebrew"));
        let mode = std::fs::metadata(&binary).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn build_prefix_relocation_refuses_a_longer_prefix() {
        let (_tmp, _cfg, keg) = keg_with(&[("bin/demo", b"\x00/opt/homebrew\x00")]);
        let err = relocate_build_prefix(
            &keg,
            "/opt/homebrew",
            "/a/much/longer/prefix",
            Some(&["bin/demo".into()]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("longer prefix"), "{err}");
    }

    #[test]
    fn relativizes_absolute_symlinks_into_the_build_prefix() {
        let (_tmp, cfg, keg) = keg_with(&[("lib/real.dylib", b"x")]);
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        std::os::unix::fs::symlink(
            "/opt/homebrew/Cellar/demo/1.0/lib/real.dylib",
            keg.join("bin/a"),
        )
        .unwrap();
        std::os::unix::fs::symlink("/opt/homebrew/share/demo", keg.join("bin/b")).unwrap();
        std::os::unix::fs::symlink("/usr/lib/libSystem.dylib", keg.join("bin/c")).unwrap();
        std::os::unix::fs::symlink("../lib/real.dylib", keg.join("bin/d")).unwrap();

        relativize_prefix_symlinks(&cfg, &keg, "/opt/homebrew", "/opt/homebrew/Cellar").unwrap();
        let rel = |p: &str| std::fs::read_link(keg.join(p)).unwrap();
        assert_eq!(
            rel("bin/a"),
            crate::keg::relative_path(
                &cfg.cellar.join("demo/1.0/lib/real.dylib"),
                &keg.join("bin")
            )
        );
        assert_eq!(
            rel("bin/b"),
            crate::keg::relative_path(&cfg.prefix.join("share/demo"), &keg.join("bin"))
        );
        // Links outside the build prefix and relative links are untouched.
        assert_eq!(rel("bin/c"), Path::new("/usr/lib/libSystem.dylib"));
        assert_eq!(rel("bin/d"), Path::new("../lib/real.dylib"));
    }

    #[test]
    fn skip_relocation_bottles_leave_mach_o_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let keg = cfg.cellar.join("demo/1.0/bin");
        std::fs::create_dir_all(&keg).unwrap();
        let keg = cfg.cellar.join("demo/1.0");
        std::fs::copy("/bin/ls", keg.join("bin/ls")).unwrap();
        let before = std::fs::read(keg.join("bin/ls")).unwrap();
        let tab = BottleTab {
            changed_files: Some(vec![]),
            ..Default::default()
        };
        let report = relocate_keg(
            &cfg,
            RelocateArgs {
                keg_path: &keg,
                cellar_kind: &BottleCellar::AnySkipRelocation,
                tab: &tab,
                openjdk_dep: None,
            },
        )
        .unwrap();
        assert!(report.macho_files_changed.is_empty());
        assert!(report.text_files_changed.is_empty());
        assert_eq!(std::fs::read(keg.join("bin/ls")).unwrap(), before);
    }

    #[test]
    fn perl_and_java_placeholders() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let mut text = b"#!@@HOMEBREW_PERL@@ -w\n@@HOMEBREW_JAVA@@/bin/java\n".to_vec();
        let repl = Replacements::new(&cfg, None, Some("openjdk@21"), None);
        assert!(repl.apply(&mut text));
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("/usr/bin/perl5."), "{text}");
        assert!(
            text.contains(&format!(
                "{}/opt/openjdk@21/libexec/openjdk.jdk/Contents/Home/bin/java",
                cfg.prefix.display()
            )),
            "{text}"
        );

        // A formula that declares perl directly gets the brewed perl.
        let tab = BottleTab {
            runtime_dependencies: vec![crate::model::RuntimeDependency {
                full_name: "perl".into(),
                declared_directly: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let repl = Replacements::new(&cfg, Some(&tab), None, None);
        let mut text = b"#!@@HOMEBREW_PERL@@\n".to_vec();
        assert!(repl.apply(&mut text));
        assert_eq!(
            String::from_utf8(text).unwrap(),
            format!("#!{}/opt/perl/bin/perl\n", cfg.prefix.display())
        );
    }

    #[test]
    fn install_names_only_expand_prefix_and_cellar() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let repl = Replacements::new(&cfg, None, None, None);
        assert_eq!(
            repl.install_name("@@HOMEBREW_PREFIX@@/opt/a/lib/liba.dylib")
                .as_deref(),
            Some(&*format!("{}/opt/a/lib/liba.dylib", cfg.prefix.display()))
        );
        assert_eq!(
            repl.install_name("@@HOMEBREW_CELLAR@@/a/1/lib/liba.dylib")
                .as_deref(),
            Some(&*format!("{}/a/1/lib/liba.dylib", cfg.cellar.display()))
        );
        assert_eq!(repl.install_name("@loader_path/liba.dylib"), None);
        assert_eq!(repl.install_name("/usr/lib/libSystem.B.dylib"), None);
        assert_eq!(repl.install_name("@@HOMEBREW_LIBRARY@@/x"), None);
    }

    #[test]
    fn falls_back_to_install_name_tool_without_a_header_pad() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let keg = cfg.cellar.join("demo/1.0");
        std::fs::create_dir_all(keg.join("lib")).unwrap();
        if !Path::new("/usr/bin/clang").exists()
            || !Path::new("/usr/bin/install_name_tool").exists()
        {
            eprintln!("skipping: no clang or install_name_tool");
            return;
        }
        std::fs::write(tmp.path().join("g.c"), "int g(void){return 7;}\n").unwrap();
        let dylib = keg.join("lib/libg.dylib");
        // `-headerpad 0` leaves no room to grow the load commands, so the
        // in-place editor must hand over to `install_name_tool`.
        assert!(
            std::process::Command::new("/usr/bin/clang")
                .args(["-dynamiclib", "-o"])
                .arg(&dylib)
                .arg(tmp.path().join("g.c"))
                .args([
                    "-install_name",
                    "@@HOMEBREW_PREFIX@@/lib/libg.dylib",
                    "-Wl,-headerpad,0",
                ])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        let repl = Replacements::new(&cfg, None, None, None);
        let map = |old: &str| repl.install_name(old);
        assert!(matches!(
            macho::rewrite_install_names(&dylib, &map),
            Err(macho::MachOError::NoHeaderPad { .. })
        ));

        let tab = BottleTab {
            changed_files: Some(vec![]),
            linkage_files: Some(vec!["lib/libg.dylib".into()]),
            ..Default::default()
        };
        let result = relocate_keg(
            &cfg,
            RelocateArgs {
                keg_path: &keg,
                cellar_kind: &BottleCellar::Any,
                tab: &tab,
                openjdk_dep: None,
            },
        );
        // `install_name_tool` refuses to grow past the header pad too (it no
        // longer relays the file out), so the pour fails the way Homebrew's
        // does when ruby-macho raises `HeaderPadError` — but the file is left
        // intact rather than half-rewritten.
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .starts_with("Failed changing install names in "),
            "{err}"
        );
        assert_eq!(
            macho::read_info(&dylib).unwrap().dylib_id.as_deref(),
            Some("@@HOMEBREW_PREFIX@@/lib/libg.dylib")
        );
    }

    #[test]
    fn relocates_a_real_dylib_and_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let keg = cfg.cellar.join("demo/1.0");
        std::fs::create_dir_all(keg.join("lib")).unwrap();
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        if !Path::new("/usr/bin/clang").exists() {
            eprintln!("skipping: no /usr/bin/clang");
            return;
        }
        std::fs::write(tmp.path().join("g.c"), "int g(void){return 7;}\n").unwrap();
        std::fs::write(
            tmp.path().join("m.c"),
            "#include <stdio.h>\nint g(void);int main(void){printf(\"%d\\n\",g());return 0;}\n",
        )
        .unwrap();
        let dylib = keg.join("lib/libg.dylib");
        assert!(
            std::process::Command::new("/usr/bin/clang")
                .args(["-dynamiclib", "-o"])
                .arg(&dylib)
                .arg(tmp.path().join("g.c"))
                .args([
                    "-install_name",
                    "@@HOMEBREW_PREFIX@@/opt/demo/lib/libg.dylib",
                    "-Wl,-headerpad_max_install_names",
                ])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        let exe = keg.join("bin/demo");
        assert!(
            std::process::Command::new("/usr/bin/clang")
                .arg("-o")
                .arg(&exe)
                .arg(tmp.path().join("m.c"))
                .arg(&dylib)
                .arg("-Wl,-headerpad_max_install_names")
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        );

        let tab = BottleTab {
            changed_files: Some(vec![]),
            ..Default::default()
        };
        let report = relocate_keg(
            &cfg,
            RelocateArgs {
                keg_path: &keg,
                cellar_kind: &BottleCellar::Any,
                tab: &tab,
                openjdk_dep: None,
            },
        )
        .unwrap();
        assert_eq!(
            report.macho_files_changed,
            vec!["bin/demo".to_string(), "lib/libg.dylib".to_string()]
        );
        let want = format!("{}/opt/demo/lib/libg.dylib", cfg.prefix.display());
        assert_eq!(
            macho::read_info(&dylib).unwrap().dylib_id,
            Some(want.clone())
        );
        assert!(
            macho::read_info(&exe)
                .unwrap()
                .linked_libraries
                .contains(&want)
        );
        assert!(codesign::verify(&dylib), "dylib signature broken");
        assert!(codesign::verify(&exe), "executable signature broken");
    }
}
