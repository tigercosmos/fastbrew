//! `link`, `unlink`, `optlink` and their records (`docs/COMPAT.md` 5).
//!
//! A port of `Keg#link`, `#link_dir`, `#resolve_any_conflicts`,
//! `#make_relative_symlink`, `#unlink`, `#optlink`, `#remove_old_aliases` and
//! `#remove_oldname_opt_records` from `Library/Homebrew/keg.rb`, plus the
//! `link_overwrite_paths` retry from `formula_installer.rb#link`.
//!
//! `link` implements Homebrew's per-directory rules (etc/bin/sbin/include/
//! share/lib/Frameworks), conflict handling (`ConflictError` message text,
//! `--overwrite`, `link_overwrite_paths` globs, `resolve_any_conflicts` for
//! symlinked directories), `--dry-run` listing, rollback on failure, the
//! `var/homebrew/linked/<name>` record, and `install-info` for info files.
//! `unlink` removes every prefix symlink resolving into the keg, prunes empty
//! directories it emptied, and removes the record. `optlink` maintains
//! `opt/<name>`, `opt/<alias>` and `opt/<oldname>` links.

use std::path::{Component, Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::keg::{Keg, make_relative_symlink, relative_path};

#[derive(Debug, Clone, Copy, Default)]
pub struct LinkOptions {
    pub overwrite: bool,
    pub dry_run: bool,
    pub verbose: bool,
}

/// Directories of a keg that are linked into the prefix
/// (`Keg.keg_link_directories`, plus `Frameworks` on macOS).
pub const KEG_LINK_DIRECTORIES: [&str; 8] = [
    "bin",
    "etc",
    "include",
    "lib",
    "sbin",
    "share",
    "var",
    "Frameworks",
];

/// Prefix directories that must survive `unlink`'s pruning
/// (`Keg.must_exist_subdirectories`).
const MUST_EXIST_SUBDIRECTORIES: [&str; 9] = [
    "bin",
    "etc",
    "include",
    "lib",
    "sbin",
    "share",
    "opt",
    "var/homebrew/linked",
    "Frameworks",
];

/// `Keg::SHARE_PATHS`: share subdirectories that stay real directories.
const SHARE_PATHS: [&str; 31] = [
    "aclocal",
    "cps",
    "doc",
    "info",
    "java",
    "locale",
    "man",
    "man/man1",
    "man/man2",
    "man/man3",
    "man/man4",
    "man/man5",
    "man/man6",
    "man/man7",
    "man/man8",
    "man/cat1",
    "man/cat2",
    "man/cat3",
    "man/cat4",
    "man/cat5",
    "man/cat6",
    "man/cat7",
    "man/cat8",
    "applications",
    "gnome",
    "gnome/help",
    "icons",
    "mime",
    "mime/packages",
    "mime-info",
    "pixmaps",
];

/// The remaining `Keg::SHARE_PATHS` entries (the array above is split only to
/// keep the constant sizes readable).
const SHARE_PATHS_EXTRA: [&str; 2] = ["postgresql", "sounds"];

/// What `link_dir` should do with one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Link,
    Mkpath,
    SkipDir,
    SkipFile,
    Info,
}

/// Number of symlinks created.
///
/// The keg's aliases come from its receipt, which is all `brew link` has to go
/// on; the installer knows the formula and passes them in with
/// [`link_with_aliases`], because it writes the receipt only once the install
/// has finished.
pub fn link(
    cfg: &Config,
    keg: &Keg,
    overwrite_globs: &[String],
    opts: LinkOptions,
) -> Result<usize> {
    let aliases = keg.receipt().map(|r| r.aliases).unwrap_or_default();
    link_with_aliases(cfg, keg, &aliases, overwrite_globs, opts)
}

/// [`link`] with the formula's aliases supplied by the caller.
pub fn link_with_aliases(
    cfg: &Config,
    keg: &Keg,
    aliases: &[String],
    overwrite_globs: &[String],
    opts: LinkOptions,
) -> Result<usize> {
    let record = cfg.linked_record(&keg.name);
    if record.is_dir() {
        let resolved = std::fs::canonicalize(&record).unwrap_or(record);
        return Err(Error::user(format!(
            "Cannot link {}\nAnother version is already linked: {}",
            keg.name,
            resolved.display()
        )));
    }

    if !opts.dry_run {
        optlink(cfg, keg, aliases, &[])?;
    }

    let mut linker = Linker::new(cfg, keg, overwrite_globs, opts);
    match linker.link_everything() {
        Ok(()) => {
            // Homebrew counts only the links `link_dir` makes: it extends just
            // those destinations with `ObserverPathnameExtension`, so neither
            // the `opt` record nor `var/homebrew/linked/<name>` adds to the
            // "N symlinks created." total (`Keg#link`, `Keg#link_dir`).
            let created = linker.created;
            if !opts.dry_run {
                linker.symlink(&cfg.linked_record(&keg.name), &keg.path)?;
            }
            linker.warn_about_backups();
            Ok(created)
        }
        Err(e) => {
            if opts.dry_run {
                return Err(e);
            }
            let _ = unlink_with_aliases(
                cfg,
                keg,
                aliases,
                LinkOptions {
                    dry_run: false,
                    ..opts
                },
            );
            linker.restore_backups();
            Err(e)
        }
    }
}

/// Number of symlinks removed.
///
/// As with [`link`], the keg's aliases come from its receipt; a caller that
/// knows the formula passes them in with [`unlink_with_aliases`].
pub fn unlink(cfg: &Config, keg: &Keg, opts: LinkOptions) -> Result<usize> {
    let aliases = keg.receipt().map(|r| r.aliases).unwrap_or_default();
    unlink_with_aliases(cfg, keg, &aliases, opts)
}

/// [`unlink`] with the formula's aliases supplied by the caller.
pub fn unlink_with_aliases(
    cfg: &Config,
    keg: &Keg,
    aliases: &[String],
    opts: LinkOptions,
) -> Result<usize> {
    let mut removed = 0usize;
    let mut dirs: Vec<PathBuf> = Vec::new();

    for dir in KEG_LINK_DIRECTORIES {
        let root = keg.path.join(dir);
        if !root.exists() {
            continue;
        }
        let mut stack = vec![root];
        while let Some(src) = stack.pop() {
            let relative = match src.strip_prefix(&keg.path) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let dst = cfg.prefix.join(relative);
            let dst_link = std::fs::symlink_metadata(&dst);
            let dst_is_symlink = dst_link
                .as_ref()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            if !dst_is_symlink && dst.is_dir() {
                dirs.push(dst.clone());
            }
            let src_is_dir = src.is_dir() && !src.is_symlink();
            let ours = dst_is_symlink && resolved_link(&dst).as_deref() == Some(src.as_path());
            if ours {
                if opts.dry_run {
                    println!("{}", dst.display());
                } else {
                    if is_info_file(&dst) {
                        uninstall_info(cfg, &dst, opts.verbose);
                    }
                    std::fs::remove_file(&dst)?;
                    if opts.verbose {
                        println!("rm {}", dst.display());
                    }
                    removed += 1;
                }
                // `Find.prune`: a linked directory hides everything below it.
                if src_is_dir {
                    continue;
                }
            }
            if src_is_dir && let Ok(entries) = std::fs::read_dir(&src) {
                for entry in entries.flatten() {
                    stack.push(entry.path());
                }
            }
        }
    }

    if !opts.dry_run {
        remove_old_aliases(cfg, keg, aliases);
        let record = cfg.linked_record(&keg.name);
        if keg.is_linked(cfg) {
            let _ = std::fs::remove_file(&record);
            rmdir_if_possible(record.parent().unwrap_or(&cfg.prefix));
        }
        let must_exist: Vec<PathBuf> = MUST_EXIST_SUBDIRECTORIES
            .iter()
            .map(|d| cfg.prefix.join(d))
            .collect();
        dirs.sort();
        dirs.dedup();
        for dir in dirs.iter().rev() {
            if must_exist.contains(dir) {
                continue;
            }
            rmdir_if_possible(dir);
        }
    }
    Ok(removed)
}

/// Maintain `opt/<name>` plus one record per alias and oldname.
pub fn optlink(cfg: &Config, keg: &Keg, aliases: &[String], oldnames: &[String]) -> Result<()> {
    remove_old_aliases(cfg, keg, aliases);
    let opt = cfg.opt_dir();
    std::fs::create_dir_all(&opt)?;

    let record = cfg.opt_record(&keg.name);
    replace_symlink(&record, &keg.path)?;
    for a in aliases {
        replace_symlink(&opt.join(a), &keg.path)?;
    }
    // Explicit oldnames plus any opt record already pointing into this rack
    // (`Keg#oldname_opt_records`).
    for old in oldnames
        .iter()
        .cloned()
        .chain(existing_oldname_records(cfg, keg))
    {
        if old == keg.name {
            continue;
        }
        replace_symlink(&opt.join(&old), &keg.path)?;
    }
    Ok(())
}

/// Remove `opt/<name>`, `var/homebrew/linked/<name>`, alias and oldname
/// records for a keg being uninstalled (`Keg#uninstall`).
pub fn remove_records(
    cfg: &Config,
    keg: &Keg,
    aliases: &[String],
    oldnames: &[String],
) -> Result<()> {
    if keg.is_optlinked(cfg) {
        let record = cfg.opt_record(&keg.name);
        let _ = std::fs::remove_file(&record);
        rmdir_if_possible(record.parent().unwrap_or(&cfg.prefix));
    }
    if keg.is_linked(cfg) {
        let record = cfg.linked_record(&keg.name);
        let _ = std::fs::remove_file(&record);
        rmdir_if_possible(record.parent().unwrap_or(&cfg.prefix));
    }
    remove_old_aliases(cfg, keg, aliases);
    let opt = cfg.opt_dir();
    for old in oldnames
        .iter()
        .cloned()
        .chain(existing_oldname_records(cfg, keg))
    {
        let record = opt.join(&old);
        if resolved_link(&record).as_deref() == Some(keg.path.as_path()) {
            let _ = std::fs::remove_file(&record);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- linking

struct Linker<'a> {
    cfg: &'a Config,
    keg: &'a Keg,
    opts: LinkOptions,
    overwrite_globs: &'a [String],
    created: usize,
    /// Conflicting files moved to `$CACHE/Backup` by a `link_overwrite_paths`
    /// match, as `(original, backup)`.
    backups: Vec<(PathBuf, PathBuf)>,
}

impl<'a> Linker<'a> {
    fn new(
        cfg: &'a Config,
        keg: &'a Keg,
        overwrite_globs: &'a [String],
        opts: LinkOptions,
    ) -> Linker<'a> {
        Linker {
            cfg,
            keg,
            opts,
            overwrite_globs,
            created: 0,
            backups: Vec::new(),
        }
    }

    fn link_everything(&mut self) -> Result<()> {
        self.link_dir("etc", &|_| Action::Mkpath)?;
        self.link_dir("bin", &|_| Action::SkipDir)?;
        self.link_dir("sbin", &|_| Action::SkipDir)?;
        self.link_dir("include", &include_rule)?;
        self.link_dir("share", &share_rule)?;
        self.link_dir("lib", &lib_rule)?;
        self.link_dir("Frameworks", &frameworks_rule)?;
        Ok(())
    }

    /// `Keg#link_dir`: walk `keg/<relative_dir>` and mirror it into the prefix.
    fn link_dir(&mut self, relative_dir: &str, rule: &dyn Fn(&str) -> Action) -> Result<()> {
        let root = self.keg.path.join(relative_dir);
        if !root.exists() {
            return Ok(());
        }
        self.walk(&root, &root, rule)
    }

    fn walk(&mut self, root: &Path, dir: &Path, rule: &dyn Fn(&str) -> Action) -> Result<()> {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
            Err(_) => return Ok(()),
        };
        entries.sort();
        for src in entries {
            let Ok(relative_src) = src.strip_prefix(&self.keg.path) else {
                continue;
            };
            let dst = self.cfg.prefix.join(relative_src);
            let relative_to_root = src
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let meta = match std::fs::symlink_metadata(&src) {
                Ok(m) => m,
                Err(_) => continue,
            };

            if meta.file_type().is_symlink() || meta.is_file() {
                let basename = src.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if basename == ".DS_Store" {
                    continue;
                }
                let resolved_src = resolved_link(&src).unwrap_or_else(|| src.clone());
                if resolved_src == dst {
                    continue;
                }
                // A symlink into another keg's `opt` at the same relative path
                // belongs to that keg (split formulae such as llvm + flang).
                if matches_opt_pattern(&self.cfg.opt_dir(), &resolved_src, relative_src) {
                    continue;
                }
                let extension = src.extension().and_then(|e| e.to_str()).unwrap_or("");
                if matches!(extension, "pyc" | "pyo")
                    && src.to_string_lossy().contains("/site-packages/")
                {
                    continue;
                }
                match rule(&relative_to_root) {
                    Action::SkipFile => continue,
                    Action::Info => {
                        if basename == "dir" {
                            continue; // historical local `dir` files
                        }
                        self.symlink(&dst, &src)?;
                        install_info(self.cfg, &dst, self.opts.verbose);
                    }
                    _ => self.symlink(&dst, &src)?,
                }
            } else if meta.is_dir() {
                let dst_meta = std::fs::symlink_metadata(&dst);
                let dst_is_real_dir = dst_meta
                    .as_ref()
                    .map(|m| m.is_dir() && !m.file_type().is_symlink())
                    .unwrap_or(false);
                if dst_is_real_dir {
                    self.walk(root, &src, rule)?;
                    continue;
                }
                // App bundles are never linked; Spotlight and `open` find them.
                if src.extension().and_then(|e| e.to_str()) == Some("app") {
                    continue;
                }
                match rule(&relative_to_root) {
                    Action::SkipDir => continue,
                    Action::Mkpath => {
                        if !self.resolve_any_conflicts(&dst)? {
                            if !self.opts.dry_run {
                                std::fs::create_dir_all(&dst)?;
                            }
                            if self.opts.verbose {
                                println!("mkdir -p {}", dst.display());
                            }
                        }
                        self.walk(root, &src, rule)?;
                    }
                    _ => {
                        if !self.resolve_any_conflicts(&dst)? {
                            self.symlink(&dst, &src)?;
                        } else {
                            self.walk(root, &src, rule)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// `Keg#resolve_any_conflicts`: a destination that is a symlink to another
    /// keg's directory becomes a real directory with that keg's files linked
    /// into it. Returns whether it did so.
    fn resolve_any_conflicts(&mut self, dst: &Path) -> Result<bool> {
        if !dst.is_symlink() {
            return Ok(false);
        }
        let Some(src) = resolved_link(dst) else {
            return Ok(false);
        };
        let Ok(meta) = std::fs::symlink_metadata(&src) else {
            // A broken symlink: just remove it.
            if !self.opts.dry_run {
                let _ = std::fs::remove_file(dst);
            }
            return Ok(false);
        };
        // Only one level of symlink is resolved: a link to a link is a file.
        if !meta.is_dir() {
            return Ok(false);
        }
        let Some(other) = keg_for(self.cfg, &src) else {
            if self.opts.verbose {
                println!(
                    "Won't resolve conflicts for symlink {} as it doesn't resolve into the Cellar.",
                    dst.display()
                );
            }
            return Ok(false);
        };
        if !self.opts.dry_run {
            std::fs::remove_file(dst)?;
            let mut linker = Linker::new(self.cfg, &other, &[], LinkOptions::default());
            let relative = src.strip_prefix(&other.path).unwrap_or(Path::new(""));
            linker.link_dir(&relative.to_string_lossy(), &|_| Action::Mkpath)?;
        }
        Ok(true)
    }

    /// `Keg#make_relative_symlink` with its conflict, dry-run and overwrite
    /// behavior.
    fn symlink(&mut self, dst: &Path, src: &Path) -> Result<()> {
        if dst.is_symlink() && resolved_link(dst).as_deref() == Some(src) {
            if self.opts.verbose {
                println!("Skipping; link already exists: {}", dst.display());
            }
            return Ok(());
        }
        if self.opts.dry_run {
            if self.opts.overwrite {
                if dst.is_symlink() {
                    println!(
                        "{} -> {}",
                        dst.display(),
                        resolved_link(dst).unwrap_or_default().display()
                    );
                } else if dst.exists() {
                    println!("{}", dst.display());
                }
            } else {
                println!("{}", dst.display());
            }
            return Ok(());
        }

        let exists = dst.exists() || dst.is_symlink();
        if self.opts.overwrite && exists {
            remove_path(dst)?;
        } else if exists {
            // A broken symlink is simply replaced; a real file is a conflict
            // unless `link_overwrite_paths` covers it.
            if dst.is_symlink() && !dst.exists() {
                let _ = std::fs::remove_file(dst);
            } else if self.overwrite_allowed(dst) {
                self.back_up(dst)?;
            } else {
                return Err(Error::user(conflict_message(self.cfg, self.keg, src, dst)));
            }
        }
        if let Some(parent) = dst.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)?;
        }
        make_relative_symlink(dst, src).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                Error::user(format!(
                    "Could not symlink {}\n{} is not writable.",
                    relative_path(src, &self.keg.path).display(),
                    dst.parent().unwrap_or(dst).display()
                ))
            } else {
                Error::user(format!(
                    "Could not symlink {}\n{e}",
                    relative_path(src, &self.keg.path).display()
                ))
            }
        })?;
        if self.opts.verbose {
            println!(
                "ln -s {} {}",
                relative_path(src, dst.parent().unwrap_or(dst)).display(),
                dst.file_name().unwrap_or_default().to_string_lossy()
            );
        }
        self.created += 1;
        Ok(())
    }

    /// `Formula#link_overwrite?`: a `link_overwrite_paths` entry covers `dst`,
    /// and `dst` does not belong to another installed keg.
    fn overwrite_allowed(&self, dst: &Path) -> bool {
        if self.overwrite_globs.is_empty() {
            return false;
        }
        // Files owned by another keg are never overwritten.
        if let Some(target) = resolved_link(dst)
            && keg_for(self.cfg, &target).is_some()
        {
            return false;
        }
        let Ok(relative) = dst.strip_prefix(&self.cfg.prefix) else {
            return false;
        };
        let to_check = relative.to_string_lossy();
        self.overwrite_globs
            .iter()
            .any(|p| link_overwrite_matches(p, &to_check))
    }

    fn back_up(&mut self, dst: &Path) -> Result<()> {
        let relative = dst.strip_prefix(&self.cfg.prefix).unwrap_or(dst);
        let backup = self.cfg.cache.join("Backup").join(relative);
        if let Some(parent) = backup.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(dst, &backup)?;
        self.backups.push((dst.to_path_buf(), backup));
        Ok(())
    }

    fn restore_backups(&mut self) {
        for (original, backup) in self.backups.drain(..) {
            if let Some(parent) = original.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::rename(&backup, &original);
        }
    }

    fn warn_about_backups(&self) {
        if self.backups.is_empty() {
            return;
        }
        crate::output::opoo("These files were overwritten during the `brew link` step:");
        for (original, _) in &self.backups {
            println!("{}", original.display());
        }
        println!();
        println!(
            "They have been backed up to: {}",
            self.cfg.cache.join("Backup").display()
        );
    }
}

/// `Keg::ConflictError#to_s`.
fn conflict_message(cfg: &Config, keg: &Keg, src: &Path, dst: &Path) -> String {
    let suggestion = match keg_for(cfg, dst) {
        Some(other) => format!(
            "is a symlink belonging to {}. You can unlink it:\n  brew unlink {}\n",
            other.name, other.name
        ),
        None => format!(
            "already exists. You may want to remove it:\n  rm '{}'\n",
            dst.display()
        ),
    };
    // `ConflictError#to_s` joins its parts with "\n"; `suggestion` already
    // ends with one, so the blank line before the next paragraph is real.
    format!(
        "Could not symlink {}\nTarget {}\n{suggestion}\n\
         To force the link and overwrite all conflicting files:\n  brew link --overwrite {}\n\n\
         To list all files that would be deleted:\n  brew link --overwrite {} --dry-run\n",
        relative_path(src, &keg.path).display(),
        dst.display(),
        keg.name,
        keg.name
    )
}

// ------------------------------------------------------------ link rules

fn include_rule(relative: &str) -> Action {
    if starts_with_postgresql_versioned(relative) {
        Action::Mkpath
    } else {
        Action::Link
    }
}

fn share_rule(relative: &str) -> Action {
    if is_info_path(relative) {
        return Action::Info;
    }
    if relative == "locale/locale.alias" || icon_theme_cache(relative) {
        return Action::SkipFile;
    }
    if localedir(relative)
        || relative.starts_with("icons/")
        || relative.starts_with("zsh")
        || relative.starts_with("fish")
        || relative.starts_with("pwsh")
        || relative.starts_with("lua/")
        || relative.starts_with("guile/")
        || starts_with_postgresql_versioned(relative)
        || relative.starts_with("pypy")
        || SHARE_PATHS.contains(&relative)
        || SHARE_PATHS_EXTRA.contains(&relative)
    {
        return Action::Mkpath;
    }
    Action::Link
}

fn lib_rule(relative: &str) -> Action {
    if relative == "charset.alias" {
        return Action::SkipFile;
    }
    let exact = ["cps", "pkgconfig", "cmake", "dtrace", "ghc", "php"];
    let prefixes = [
        "gdk-pixbuf",
        "gio",
        "lua",
        "mecab",
        "node",
        "ocaml",
        "perl5",
        "pypy",
        "R",
        "ruby",
    ];
    if exact.contains(&relative)
        || prefixes.iter().any(|p| relative.starts_with(p))
        || starts_with_postgresql_versioned(relative)
        || starts_with_python_versioned(relative)
    {
        return Action::Mkpath;
    }
    Action::Link
}

fn frameworks_rule(relative: &str) -> Action {
    // `%r{[^/]*\.framework(/Versions)?$}`
    let candidate = relative.strip_suffix("/Versions").unwrap_or(relative);
    let last = candidate.rsplit('/').next().unwrap_or(candidate);
    if last.ends_with(".framework") {
        Action::Mkpath
    } else {
        Action::Link
    }
}

/// `Keg::INFOFILE_RX` = `info/(?:[^.].*?\.info(?:\.gz)?|dir)$`.
fn is_info_path(relative: &str) -> bool {
    let Some(idx) = relative.rfind("info/") else {
        return false;
    };
    let rest = &relative[idx + "info/".len()..];
    if rest == "dir" {
        return true;
    }
    if rest.starts_with('.') || rest.is_empty() {
        return false;
    }
    rest.ends_with(".info") || rest.ends_with(".info.gz")
}

fn is_info_file(path: &Path) -> bool {
    is_info_path(&path.to_string_lossy())
}

/// `Keg::LOCALEDIR_RX`: `(?:locale|man)/<language>[_TERRITORY][.codeset][@modifier]`.
fn localedir(relative: &str) -> bool {
    for marker in ["locale/", "man/"] {
        let mut search = relative;
        while let Some(idx) = search.find(marker) {
            let rest = &search[idx + marker.len()..];
            let segment = rest.split('/').next().unwrap_or(rest);
            if is_locale_segment(segment) {
                return true;
            }
            search = &search[idx + marker.len()..];
        }
    }
    false
}

fn is_locale_segment(segment: &str) -> bool {
    let (language, rest) = match segment.split_once(['_', '.', '@']) {
        Some((l, _)) => (l, true),
        None => (segment, false),
    };
    let _ = rest;
    language == "C"
        || language == "POSIX"
        || (language.len() == 2 && language.bytes().all(|b| b.is_ascii_lowercase()))
}

fn icon_theme_cache(relative: &str) -> bool {
    relative.starts_with("icons/") && relative.ends_with("/icon-theme.cache")
}

fn starts_with_postgresql_versioned(relative: &str) -> bool {
    let Some(rest) = relative.strip_prefix("postgresql@") else {
        return false;
    };
    !rest.is_empty() && rest.bytes().next().is_some_and(|b| b.is_ascii_digit())
}

/// `/^python[23]\.\d+/`.
fn starts_with_python_versioned(relative: &str) -> bool {
    let Some(rest) = relative.strip_prefix("python") else {
        return false;
    };
    let mut chars = rest.chars();
    if !matches!(chars.next(), Some('2') | Some('3')) {
        return false;
    }
    if chars.next() != Some('.') {
        return false;
    }
    chars.next().is_some_and(|c| c.is_ascii_digit())
}

/// `Formula.link_overwrite_paths` matching (`formula.rb#link_overwrite?`).
pub fn link_overwrite_matches(pattern: &str, to_check: &str) -> bool {
    if pattern == to_check {
        return true;
    }
    if to_check.starts_with(&format!("{}/", pattern.trim_end_matches('/'))) {
        return true;
    }
    let escaped = regex::escape(pattern).replace("\\*", ".*?");
    regex::Regex::new(&format!("^{escaped}$")).is_ok_and(|re| re.is_match(to_check))
}

// ------------------------------------------------------------- alias work

/// `Keg#remove_old_aliases`.
fn remove_old_aliases(cfg: &Config, keg: &Keg, aliases: &[String]) {
    let opt = cfg.opt_dir();
    let linked = cfg.linked_kegs();
    let opt_record = cfg.opt_record(&keg.name);
    let linked_record = cfg.linked_record(&keg.name);

    for a in aliases {
        // Versioned aliases are handled by the glob below.
        if is_versioned_alias(a) {
            continue;
        }
        remove_alias_symlink(&opt.join(a), &opt_record, false);
        remove_alias_symlink(&linked.join(a), &linked_record, false);
    }

    let rack = keg.path.parent().map(Path::to_path_buf).unwrap_or_default();
    let versioned_prefix = format!("{}@", keg.name);
    if let Ok(entries) = std::fs::read_dir(&opt) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&versioned_prefix) || aliases.contains(&name) {
                continue;
            }
            remove_alias_symlink(&opt.join(&name), &rack, true);
            remove_alias_symlink(&linked.join(&name), &rack, true);
        }
    }
}

/// `Keg#remove_alias_symlink`.
fn remove_alias_symlink(alias_symlink: &Path, alias_match_path: &Path, match_parent: bool) {
    let is_symlink = alias_symlink.is_symlink();
    if is_symlink && alias_symlink.exists() {
        let Ok(mut real) = std::fs::canonicalize(alias_symlink) else {
            return;
        };
        if match_parent {
            real = real.parent().map(Path::to_path_buf).unwrap_or(real);
        }
        if let Ok(match_real) = std::fs::canonicalize(alias_match_path)
            && real == match_real
        {
            let _ = std::fs::remove_file(alias_symlink);
        }
    } else if is_symlink || alias_symlink.exists() {
        let _ = remove_path(alias_symlink);
    }
}

fn is_versioned_alias(name: &str) -> bool {
    // `/.+@./`: at least one character either side of an `@`.
    match name.split_once('@') {
        Some((before, after)) => !before.is_empty() && !after.is_empty(),
        None => false,
    }
}

/// `Keg#oldname_opt_records`: opt records other than ours already pointing
/// into this keg's rack.
fn existing_oldname_records(cfg: &Config, keg: &Keg) -> Vec<String> {
    let opt = cfg.opt_dir();
    let Some(rack) = keg.path.parent() else {
        return vec![];
    };
    let Ok(entries) = std::fs::read_dir(&opt) else {
        return vec![];
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_symlink() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == keg.name {
                return None;
            }
            let target = resolved_link(&path)?;
            (target.parent() == Some(rack)).then_some(name)
        })
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------- helpers

/// `Utils::Path.resolved_path`: one level of symlink, cleaned lexically.
fn resolved_link(path: &Path) -> Option<PathBuf> {
    if !path.is_symlink() {
        return Some(path.to_path_buf());
    }
    let target = std::fs::read_link(path).ok()?;
    if target.is_absolute() {
        return Some(cleanpath(&target));
    }
    Some(cleanpath(&path.parent()?.join(target)))
}

/// `Pathname#cleanpath`: resolve `.` and `..` lexically.
fn cleanpath(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `resolved_src.fnmatch?("#{HOMEBREW_PREFIX}/opt/*/#{relative_src}", FNM_PATHNAME)`.
fn matches_opt_pattern(opt_dir: &Path, resolved_src: &Path, relative_src: &Path) -> bool {
    let Ok(rest) = resolved_src.strip_prefix(opt_dir) else {
        return false;
    };
    let mut components = rest.components();
    // `*` under FNM_PATHNAME matches exactly one path component.
    if components.next().is_none() {
        return false;
    }
    components.as_path() == relative_src
}

/// The keg a prefix path resolves into, if any (`Keg.for`). The returned keg's
/// path is spelled the way `cfg.cellar` is, not canonicalized, so it compares
/// equal to the other paths this module builds.
fn keg_for(cfg: &Config, path: &Path) -> Option<Keg> {
    let real = std::fs::canonicalize(path).ok()?;
    let cellar = std::fs::canonicalize(&cfg.cellar).ok()?;
    let mut current = real.as_path();
    loop {
        if current.parent()?.parent()? == cellar {
            let version = current.file_name()?.to_str()?;
            let name = current.parent()?.file_name()?.to_str()?;
            return Some(Keg::new(cfg, name, version));
        }
        current = current.parent()?;
    }
}

fn replace_symlink(record: &Path, target: &Path) -> Result<()> {
    if record.is_symlink() || record.exists() {
        remove_path(record)?;
    }
    if let Some(parent) = record.parent() {
        std::fs::create_dir_all(parent)?;
    }
    make_relative_symlink(record, target)?;
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// `rmdir_if_possible`: removes an empty directory, retrying once after a
/// lone `.DS_Store`.
fn rmdir_if_possible(dir: &Path) -> bool {
    if std::fs::remove_dir(dir).is_ok() {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let children: Vec<_> = entries.flatten().collect();
    if children.len() == 1 && children[0].file_name() == ".DS_Store" {
        let _ = std::fs::remove_file(children[0].path());
        return std::fs::remove_dir(dir).is_ok();
    }
    false
}

/// `/usr/bin/install-info`, or the brewed texinfo's.
fn install_info_executable(cfg: &Config) -> Option<PathBuf> {
    let system = PathBuf::from("/usr/bin/install-info");
    if is_executable(&system) {
        return Some(system);
    }
    let brewed = cfg.opt_record("texinfo").join("bin/install-info");
    is_executable(&brewed).then_some(brewed)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn install_info(cfg: &Config, path: &Path, verbose: bool) {
    run_install_info(cfg, path, &["--quiet"], verbose, "info");
}

fn uninstall_info(cfg: &Config, path: &Path, verbose: bool) {
    run_install_info(cfg, path, &["--delete", "--quiet"], verbose, "uninfo");
}

fn run_install_info(cfg: &Config, path: &Path, flags: &[&str], verbose: bool, label: &str) {
    let Some(exe) = install_info_executable(cfg) else {
        return;
    };
    let dir = path.parent().unwrap_or(Path::new(".")).join("dir");
    let _ = std::process::Command::new(exe)
        .args(flags)
        .arg(path)
        .arg(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if verbose {
        println!("{label} {}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        _tmp: tempfile::TempDir,
        cfg: Config,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let cfg = Config::for_test(tmp.path());
            for dir in [
                "bin",
                "sbin",
                "etc",
                "include",
                "lib",
                "share",
                "Frameworks",
                "opt",
            ] {
                std::fs::create_dir_all(cfg.prefix.join(dir)).unwrap();
            }
            std::fs::create_dir_all(cfg.linked_kegs()).unwrap();
            Fixture { _tmp: tmp, cfg }
        }

        /// Create `$CELLAR/<name>/<version>` containing `files`.
        fn keg(&self, name: &str, version: &str, files: &[&str]) -> Keg {
            let keg = Keg::new(&self.cfg, name, version);
            for rel in files {
                let path = keg.path.join(rel);
                if rel.ends_with('/') {
                    std::fs::create_dir_all(&path).unwrap();
                    continue;
                }
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, format!("{name} {rel}\n")).unwrap();
                if rel.starts_with("bin/") || rel.starts_with("sbin/") {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                        .unwrap();
                }
            }
            std::fs::write(
                keg.path.join("INSTALL_RECEIPT.json"),
                br#"{"aliases":[],"source":{}}"#,
            )
            .unwrap();
            keg
        }
    }

    #[test]
    fn links_bin_and_share_with_homebrew_rules() {
        let f = Fixture::new();
        let keg = f.keg(
            "demo",
            "1.0",
            &[
                "bin/demo",
                "bin/sub/nested",
                "share/demo/data.txt",
                "share/man/man1/demo.1",
                "share/locale/de/LC_MESSAGES/demo.mo",
                "share/zsh/site-functions/_demo",
                "lib/libdemo.dylib",
                "lib/pkgconfig/demo.pc",
                "lib/charset.alias",
                "etc/demo.conf",
                "include/demo.h",
            ],
        );
        let n = link(&f.cfg, &keg, &[], LinkOptions::default()).unwrap();
        assert!(n > 0);
        let p = &f.cfg.prefix;

        // `bin` links files but never descends into subdirectories.
        assert!(p.join("bin/demo").is_symlink());
        assert!(!p.join("bin/sub").exists());
        // `share/<other>` is linked as a directory.
        assert!(p.join("share/demo").is_symlink());
        // `man`, `locale` and `zsh` stay real directories.
        assert!(p.join("share/man/man1").is_dir() && !p.join("share/man/man1").is_symlink());
        assert!(p.join("share/man/man1/demo.1").is_symlink());
        // `LOCALEDIR_RX` is unanchored, so everything under `locale/<lang>`
        // stays a real directory too (as in the host prefix).
        assert!(p.join("share/locale/de").is_dir() && !p.join("share/locale/de").is_symlink());
        assert!(
            p.join("share/locale/de/LC_MESSAGES").is_dir()
                && !p.join("share/locale/de/LC_MESSAGES").is_symlink()
        );
        assert!(p.join("share/locale/de/LC_MESSAGES/demo.mo").is_symlink());
        assert!(
            p.join("share/zsh/site-functions").is_dir()
                && !p.join("share/zsh/site-functions").is_symlink()
        );
        assert!(p.join("share/zsh/site-functions/_demo").is_symlink());
        // `lib` links dylibs but makes `pkgconfig` a real directory.
        assert!(p.join("lib/libdemo.dylib").is_symlink());
        assert!(p.join("lib/pkgconfig").is_dir() && !p.join("lib/pkgconfig").is_symlink());
        assert!(p.join("lib/pkgconfig/demo.pc").is_symlink());
        // `lib/charset.alias` is skipped.
        assert!(!p.join("lib/charset.alias").exists());
        // `etc` is mkpath plus linked files; `include` is linked.
        assert!(p.join("etc/demo.conf").is_symlink());
        assert!(p.join("include/demo.h").is_symlink());

        // Links are relative and the record exists.
        assert_eq!(
            std::fs::read_link(p.join("bin/demo")).unwrap(),
            Path::new("../Cellar/demo/1.0/bin/demo")
        );
        assert!(keg.is_linked(&f.cfg));
        assert!(keg.is_optlinked(&f.cfg));
    }

    #[test]
    fn unlink_removes_links_and_prunes_directories() {
        let f = Fixture::new();
        let keg = f.keg(
            "demo",
            "1.0",
            &["bin/demo", "share/man/man1/demo.1", "share/demo/data.txt"],
        );
        link(&f.cfg, &keg, &[], LinkOptions::default()).unwrap();
        let p = f.cfg.prefix.clone();
        assert!(p.join("share/man/man1/demo.1").is_symlink());

        let removed = unlink(&f.cfg, &keg, LinkOptions::default()).unwrap();
        assert!(removed >= 3, "removed {removed}");
        assert!(!p.join("bin/demo").exists());
        assert!(!p.join("share/demo").exists());
        // Emptied directories are pruned, but the standard ones survive.
        assert!(!p.join("share/man/man1").exists());
        assert!(p.join("share").is_dir());
        assert!(p.join("bin").is_dir());
        assert!(!keg.is_linked(&f.cfg));
        // `opt` is left in place: only `unlink` ran, not `uninstall`.
        assert!(keg.is_optlinked(&f.cfg));
    }

    #[test]
    fn conflicts_abort_with_homebrew_wording_and_roll_back() {
        let f = Fixture::new();
        let keg = f.keg("demo", "1.0", &["bin/demo", "bin/other", "include/demo.h"]);
        std::fs::write(f.cfg.prefix.join("bin/other"), b"someone else\n").unwrap();

        let err = link(&f.cfg, &keg, &[], LinkOptions::default()).unwrap_err();
        let dst = f.cfg.prefix.join("bin/other");
        // Byte-for-byte `Keg::ConflictError#to_s`.
        assert_eq!(
            err.to_string(),
            format!(
                "Could not symlink bin/other\n\
                 Target {}\n\
                 already exists. You may want to remove it:\n  rm '{}'\n\n\
                 To force the link and overwrite all conflicting files:\n  brew link --overwrite demo\n\n\
                 To list all files that would be deleted:\n  brew link --overwrite demo --dry-run\n",
                dst.display(),
                dst.display()
            )
        );
        // Everything linked before the conflict was rolled back.
        assert!(!f.cfg.prefix.join("bin/demo").exists());
        assert!(!f.cfg.prefix.join("include/demo.h").exists());
        assert!(!keg.is_linked(&f.cfg));
        // The conflicting file is untouched.
        assert_eq!(
            std::fs::read_to_string(f.cfg.prefix.join("bin/other")).unwrap(),
            "someone else\n"
        );
    }

    #[test]
    fn a_conflict_with_another_keg_names_it() {
        let f = Fixture::new();
        let other = f.keg("other", "2.0", &["bin/shared"]);
        link(&f.cfg, &other, &[], LinkOptions::default()).unwrap();
        let keg = f.keg("demo", "1.0", &["bin/shared"]);
        let err = link(&f.cfg, &keg, &[], LinkOptions::default()).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains(
                "is a symlink belonging to other. You can unlink it:\n  brew unlink other"
            ),
            "{text}"
        );
    }

    #[test]
    fn overwrite_replaces_conflicting_files() {
        let f = Fixture::new();
        let keg = f.keg("demo", "1.0", &["bin/demo"]);
        std::fs::write(f.cfg.prefix.join("bin/demo"), b"old\n").unwrap();
        let n = link(
            &f.cfg,
            &keg,
            &[],
            LinkOptions {
                overwrite: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(n, 1, "only bin/demo counts, not the linked-keg record");
        assert!(f.cfg.prefix.join("bin/demo").is_symlink());
    }

    #[test]
    fn link_overwrite_paths_back_up_the_conflict() {
        let f = Fixture::new();
        let keg = f.keg("demo", "1.0", &["bin/demo", "share/demo/x"]);
        std::fs::write(f.cfg.prefix.join("bin/demo"), b"old\n").unwrap();

        // A non-matching glob still conflicts.
        let err = link(
            &f.cfg,
            &keg,
            &["bin/nope".to_string()],
            LinkOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().starts_with("Could not symlink bin/demo"));

        let n = link(&f.cfg, &keg, &["bin/*".to_string()], LinkOptions::default()).unwrap();
        assert!(n >= 2);
        assert!(f.cfg.prefix.join("bin/demo").is_symlink());
        assert_eq!(
            std::fs::read_to_string(f.cfg.cache.join("Backup/bin/demo")).unwrap(),
            "old\n"
        );
    }

    #[test]
    fn dry_run_creates_nothing() {
        let f = Fixture::new();
        let keg = f.keg("demo", "1.0", &["bin/demo", "share/demo/x"]);
        let n = link(
            &f.cfg,
            &keg,
            &[],
            LinkOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(n, 0);
        assert!(!f.cfg.prefix.join("bin/demo").exists());
        assert!(!keg.is_optlinked(&f.cfg));
        assert!(!keg.is_linked(&f.cfg));
    }

    #[test]
    fn resolve_any_conflicts_turns_a_keg_symlink_into_a_directory() {
        let f = Fixture::new();
        let other = f.keg("other", "2.0", &["share/things/a.txt"]);
        link(&f.cfg, &other, &[], LinkOptions::default()).unwrap();
        // `share/things` is a symlink into `other`.
        assert!(f.cfg.prefix.join("share/things").is_symlink());

        let keg = f.keg("demo", "1.0", &["share/things/b.txt"]);
        link(&f.cfg, &keg, &[], LinkOptions::default()).unwrap();

        let dir = f.cfg.prefix.join("share/things");
        assert!(
            dir.is_dir() && !dir.is_symlink(),
            "should now be a real directory"
        );
        assert!(
            dir.join("a.txt").is_symlink(),
            "other's file must be relinked"
        );
        assert!(dir.join("b.txt").is_symlink());
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "other share/things/a.txt\n"
        );
    }

    #[test]
    fn already_linked_other_version_is_refused() {
        let f = Fixture::new();
        let old = f.keg("demo", "1.0", &["bin/demo"]);
        link(&f.cfg, &old, &[], LinkOptions::default()).unwrap();
        let new = f.keg("demo", "2.0", &["bin/demo"]);
        let err = link(&f.cfg, &new, &[], LinkOptions::default()).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("Cannot link demo\nAnother version is already linked: "),
            "{err}"
        );
    }

    #[test]
    fn optlink_creates_aliases_and_oldnames() {
        let f = Fixture::new();
        let keg = f.keg("demo", "1.0", &["bin/demo"]);
        optlink(&f.cfg, &keg, &["demo-alias".into()], &["olddemo".into()]).unwrap();
        for name in ["demo", "demo-alias", "olddemo"] {
            let record = f.cfg.opt_record(name);
            assert!(record.is_symlink(), "{name} missing");
            assert_eq!(
                std::fs::read_link(&record).unwrap(),
                Path::new("../Cellar/demo/1.0")
            );
        }
        // Re-running moves every record to the new keg.
        let new = f.keg("demo", "2.0", &["bin/demo"]);
        optlink(&f.cfg, &new, &["demo-alias".into()], &[]).unwrap();
        for name in ["demo", "demo-alias", "olddemo"] {
            assert_eq!(
                std::fs::read_link(f.cfg.opt_record(name)).unwrap(),
                Path::new("../Cellar/demo/2.0"),
                "{name} not moved"
            );
        }
    }

    #[test]
    fn remove_records_clears_opt_linked_and_oldnames() {
        let f = Fixture::new();
        let keg = f.keg("demo", "1.0", &["bin/demo"]);
        optlink(&f.cfg, &keg, &["demo-alias".into()], &["olddemo".into()]).unwrap();
        link(&f.cfg, &keg, &[], LinkOptions::default()).unwrap();
        remove_records(&f.cfg, &keg, &["demo-alias".into()], &["olddemo".into()]).unwrap();
        assert!(!f.cfg.opt_record("demo").exists());
        assert!(!f.cfg.opt_record("demo-alias").is_symlink());
        assert!(!f.cfg.opt_record("olddemo").is_symlink());
        assert!(!f.cfg.linked_record("demo").is_symlink());
    }

    #[test]
    fn skips_files_that_belong_to_another_kegs_opt_path() {
        let f = Fixture::new();
        let keg = f.keg("demo", "1.0", &["lib/plugins/"]);
        // `lib/shared.dylib` points at another keg's identically-placed file.
        let other = f.keg("other", "2.0", &["lib/shared.dylib"]);
        optlink(&f.cfg, &other, &[], &[]).unwrap();
        std::os::unix::fs::symlink(
            f.cfg.opt_record("other").join("lib/shared.dylib"),
            keg.path.join("lib/shared.dylib"),
        )
        .unwrap();
        link(&f.cfg, &keg, &[], LinkOptions::default()).unwrap();
        assert!(
            !f.cfg.prefix.join("lib/shared.dylib").exists(),
            "must not steal another keg's relative link"
        );
    }

    #[test]
    fn skips_ds_store_app_bundles_and_pyc_files() {
        let f = Fixture::new();
        let keg = f.keg(
            "demo",
            "1.0",
            &[
                "bin/.DS_Store",
                "bin/demo",
                "lib/Demo.app/Contents/x",
                "lib/python3.12/site-packages/mod.pyc",
                "lib/python3.12/site-packages/mod.py",
            ],
        );
        link(&f.cfg, &keg, &[], LinkOptions::default()).unwrap();
        let p = &f.cfg.prefix;
        assert!(!p.join("bin/.DS_Store").exists());
        assert!(p.join("bin/demo").is_symlink());
        assert!(!p.join("lib/Demo.app").exists());
        // `/^python[23]\.\d+/` is unanchored, so `site-packages` is a real
        // directory too; its `.py` file is linked and its `.pyc` is skipped.
        assert!(p.join("lib/python3.12").is_dir() && !p.join("lib/python3.12").is_symlink());
        assert!(
            p.join("lib/python3.12/site-packages").is_dir()
                && !p.join("lib/python3.12/site-packages").is_symlink()
        );
        assert!(p.join("lib/python3.12/site-packages/mod.py").is_symlink());
        assert!(!p.join("lib/python3.12/site-packages/mod.pyc").exists());
    }

    #[test]
    fn frameworks_versions_are_mkpath() {
        let f = Fixture::new();
        let keg = f.keg(
            "demo",
            "1.0",
            &["Frameworks/Demo.framework/Versions/A/Demo"],
        );
        link(&f.cfg, &keg, &[], LinkOptions::default()).unwrap();
        let p = &f.cfg.prefix;
        let fw = p.join("Frameworks/Demo.framework");
        assert!(fw.is_dir() && !fw.is_symlink());
        assert!(fw.join("Versions").is_dir() && !fw.join("Versions").is_symlink());
        assert!(fw.join("Versions/A").is_symlink());
    }

    #[test]
    fn info_files_are_recognised() {
        assert!(is_info_path("info/demo.info"));
        assert!(is_info_path("info/demo.info.gz"));
        assert!(is_info_path("info/dir"));
        assert!(is_info_path("emacs/info/x.info"));
        assert!(!is_info_path("info/.hidden.info"));
        assert!(!is_info_path("info/demo.txt"));
        assert!(!is_info_path("man/man1/demo.1"));
    }

    #[test]
    fn share_and_lib_rules() {
        assert_eq!(share_rule("info/x.info"), Action::Info);
        assert_eq!(share_rule("locale/locale.alias"), Action::SkipFile);
        assert_eq!(
            share_rule("icons/hicolor/icon-theme.cache"),
            Action::SkipFile
        );
        assert_eq!(share_rule("icons/hicolor"), Action::Mkpath);
        assert_eq!(share_rule("locale/de_DE.UTF-8"), Action::Mkpath);
        assert_eq!(share_rule("man/man3"), Action::Mkpath);
        assert_eq!(share_rule("aclocal"), Action::Mkpath);
        assert_eq!(share_rule("postgresql@16/extension"), Action::Mkpath);
        assert_eq!(share_rule("fish/vendor_completions.d"), Action::Mkpath);
        assert_eq!(share_rule("demo"), Action::Link);
        assert_eq!(share_rule("javadoc"), Action::Link);

        assert_eq!(lib_rule("charset.alias"), Action::SkipFile);
        assert_eq!(lib_rule("pkgconfig"), Action::Mkpath);
        assert_eq!(lib_rule("python3.12"), Action::Mkpath);
        assert_eq!(lib_rule("python3"), Action::Link);
        assert_eq!(lib_rule("ruby/gems"), Action::Mkpath);
        assert_eq!(lib_rule("libdemo.dylib"), Action::Link);
        assert_eq!(lib_rule("postgresql@16"), Action::Mkpath);

        assert_eq!(include_rule("postgresql@16"), Action::Mkpath);
        assert_eq!(include_rule("demo.h"), Action::Link);

        assert_eq!(frameworks_rule("Demo.framework"), Action::Mkpath);
        assert_eq!(frameworks_rule("Demo.framework/Versions"), Action::Mkpath);
        assert_eq!(frameworks_rule("Demo.framework/Versions/A"), Action::Link);
    }

    #[test]
    fn link_overwrite_glob_matching() {
        assert!(link_overwrite_matches("bin/demo", "bin/demo"));
        assert!(link_overwrite_matches("bin", "bin/demo"));
        assert!(link_overwrite_matches("bin/", "bin/demo"));
        assert!(link_overwrite_matches("bin/*", "bin/demo"));
        assert!(link_overwrite_matches(
            "share/man/man1/*.1",
            "share/man/man1/demo.1"
        ));
        assert!(!link_overwrite_matches("bin/*", "sbin/demo"));
        assert!(!link_overwrite_matches("bin/demo", "bin/demos"));
    }

    #[test]
    fn relinking_an_existing_link_is_idempotent() {
        let f = Fixture::new();
        let keg = f.keg("demo", "1.0", &["bin/demo"]);
        link(&f.cfg, &keg, &[], LinkOptions::default()).unwrap();
        unlink(&f.cfg, &keg, LinkOptions::default()).unwrap();
        let n = link(&f.cfg, &keg, &[], LinkOptions::default()).unwrap();
        assert!(n >= 1);
        assert!(f.cfg.prefix.join("bin/demo").is_symlink());
    }

    #[test]
    fn broken_destination_symlinks_are_replaced() {
        let f = Fixture::new();
        let keg = f.keg("demo", "1.0", &["bin/demo"]);
        std::os::unix::fs::symlink("/nonexistent/thing", f.cfg.prefix.join("bin/demo")).unwrap();
        link(&f.cfg, &keg, &[], LinkOptions::default()).unwrap();
        assert_eq!(
            std::fs::read_link(f.cfg.prefix.join("bin/demo")).unwrap(),
            Path::new("../Cellar/demo/1.0/bin/demo")
        );
    }
}
