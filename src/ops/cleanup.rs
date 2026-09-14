//! `cleanup` (port of `Library/Homebrew/cleanup.rb`): remove kegs that are not
//! the latest installed version and not linked or pinned, remove cached
//! downloads older than `HOMEBREW_CLEANUP_MAX_AGE_DAYS` or not matching the
//! current version (`--prune=days`, `-s` scrub removes all downloads), prune
//! old logs, remove stale lock files, and print the freed space summary
//! `==> This operation has freed approximately 1.2MB of disk space.`

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::api::index::Index;
use crate::cask;
use crate::config::Config;
use crate::error::Result;
use crate::keg::{self, Keg};
use crate::output;
use crate::version::PkgVersion;

/// `Cleanup::CLEANUP_DEFAULT_DAYS` (`HOMEBREW_CLEANUP_PERIODIC_FULL_DAYS`).
const CLEANUP_DEFAULT_DAYS: u64 = 30;
/// `cleanup_logs` prunes with `[days, CLEANUP_DEFAULT_DAYS].min`, which with the
/// default `cleanup_max_age_days` of 120 is 30. Homebrew's own docs say two
/// weeks; the code says otherwise and the code wins.
const LOG_PRUNE_DAYS: u64 = CLEANUP_DEFAULT_DAYS;

#[derive(Debug, Clone, Default)]
pub struct CleanupOptions {
    pub dry_run: bool,
    pub scrub: bool,
    pub prune_days: Option<u64>,
    pub prune_prefix: bool,
}

/// Accumulates what was (or would be) removed, like `Cleanup#disk_cleanup_size`.
struct Sweeper<'a> {
    cfg: &'a Config,
    opts: &'a CleanupOptions,
    freed: u64,
    seen: HashSet<PathBuf>,
    quiet: bool,
}

impl<'a> Sweeper<'a> {
    fn new(cfg: &'a Config, opts: &'a CleanupOptions) -> Sweeper<'a> {
        Sweeper {
            cfg,
            opts,
            freed: 0,
            seen: HashSet::new(),
            quiet: false,
        }
    }

    fn days(&self) -> u64 {
        self.opts
            .prune_days
            .unwrap_or(self.cfg.cleanup_max_age_days)
    }

    /// `Cleanup#cleanup_path`.
    fn remove(&mut self, path: &Path, remove: impl FnOnce() -> std::io::Result<()>) {
        if !path.exists() && !path.is_symlink() {
            return;
        }
        if !self.seen.insert(path.to_path_buf()) {
            return;
        }
        let (files, bytes) = keg::disk_usage(path);
        self.freed += bytes;
        let abv = crate::cli::fmt::abv(files, bytes);
        if self.opts.dry_run {
            if !self.quiet {
                println!("Would remove: {} ({abv})", path.display());
            }
        } else {
            if !self.quiet {
                println!("Removing: {}... ({abv})", path.display());
            }
            let _ = remove();
        }
    }
}

/// Empty `names` cleans everything.
pub fn cleanup(cfg: &Config, index: &Index, names: &[String], opts: &CleanupOptions) -> Result<()> {
    let mut sweeper = Sweeper::new(cfg, opts);
    if opts.prune_prefix {
        prune_prefix_symlinks_and_directories(cfg, opts);
        return Ok(());
    }

    if names.is_empty() {
        let mut installed = keg::installed_formula_names(cfg);
        installed.sort();
        for name in &installed {
            cleanup_formula(&mut sweeper, index, name);
        }
        for installed_cask in cask::installed_casks(cfg) {
            cleanup_cask(&mut sweeper, index, &installed_cask.token);
        }
        cleanup_cache(&mut sweeper, index);
        cleanup_logs(&mut sweeper);
        cleanup_lockfiles(cfg, opts);
        prune_prefix_symlinks_and_directories(cfg, opts);
    } else {
        for name in names {
            if index.has_formula(name) || cfg.rack(name).is_dir() {
                cleanup_formula(&mut sweeper, index, name);
            }
            if index.has_cask(name) || cask::installed_cask(cfg, name).is_some() {
                cleanup_cask(&mut sweeper, index, name);
            }
        }
    }

    if sweeper.freed > 0 {
        let size = keg::disk_usage_readable(sweeper.freed);
        if opts.dry_run {
            output::ohai(&format!(
                "This operation would free approximately {size} of disk space."
            ));
        } else {
            output::ohai(&format!(
                "This operation has freed approximately {size} of disk space."
            ));
        }
    }
    Ok(())
}

/// Remove older kegs and unreferenced downloads for the given formulae after an
/// install or upgrade (`Cleanup.install_formula_clean!`).
pub fn cleanup_after_install(cfg: &Config, index: &Index, names: &[String]) -> Result<()> {
    if cfg.no_install_cleanup {
        return Ok(());
    }
    let opts = CleanupOptions::default();
    for name in names {
        // Run quietly first so the `==> Running ...` heading is only printed
        // when there is something to say, as `install_formula_clean!` does.
        let mut probe = Sweeper::new(cfg, &opts);
        probe.quiet = true;
        cleanup_formula(&mut probe, index, name);
        if probe.seen.is_empty() {
            continue;
        }
        output::ohai(&format!("Running `brew cleanup {name}`..."));
        print_no_install_cleanup_disable_message(cfg);
        let mut sweeper = Sweeper::new(cfg, &opts);
        cleanup_formula(&mut sweeper, index, name);
    }
    Ok(())
}

/// `Cleanup.puts_no_install_cleanup_disable_message_if_not_already!`, printed
/// at most once per process.
fn print_no_install_cleanup_disable_message(cfg: &Config) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static PRINTED: AtomicBool = AtomicBool::new(false);
    if cfg.no_env_hints || cfg.no_install_cleanup {
        return;
    }
    if PRINTED.swap(true, Ordering::Relaxed) {
        return;
    }
    println!("Disable this behaviour by setting `HOMEBREW_NO_INSTALL_CLEANUP=1`.");
    println!("Hide these hints with `HOMEBREW_NO_ENV_HINTS=1` (see `man brew`).");
}

/// `Cleanup#cleanup_formula`.
fn cleanup_formula(sweeper: &mut Sweeper<'_>, index: &Index, name: &str) {
    for keg in eligible_kegs_for_cleanup(sweeper.cfg, index, name, sweeper.quiet) {
        let cfg = sweeper.cfg;
        let aliases = index
            .formula(&keg.name)
            .map(|e| e.aliases)
            .or_else(|| keg.receipt().ok().map(|r| r.aliases))
            .unwrap_or_default();
        let oldnames = index
            .formula(&keg.name)
            .map(|e| e.oldnames)
            .unwrap_or_default();
        let path = keg.path.clone();
        sweeper.remove(&path, || {
            let _ = keg::link::unlink(cfg, &keg, keg::link::LinkOptions::default());
            std::fs::remove_dir_all(&keg.path)?;
            let _ = keg::link::remove_records(cfg, &keg, &aliases, &oldnames);
            let _ = std::fs::remove_dir(cfg.rack(&keg.name));
            Ok(())
        });
    }
    cleanup_formula_downloads(sweeper, index, name);
}

/// `Formula#eligible_kegs_for_cleanup`: every keg older than the newest one
/// that is neither linked nor pinned.
pub fn eligible_kegs_for_cleanup(cfg: &Config, index: &Index, name: &str, quiet: bool) -> Vec<Keg> {
    let kegs = keg::installed_kegs(cfg, name);
    if kegs.is_empty() {
        return vec![];
    }
    let Some(current) = index.formula_pkg_version(name) else {
        // Not in the API: keep everything but the newest keg only when a
        // newer one is clearly installed, which `latest_version_installed?`
        // cannot decide. Homebrew skips the rack entirely.
        return vec![];
    };
    let current = PkgVersion::parse(&current);
    let current_scheme = index.formula_version_scheme(name);
    let latest_installed = kegs.iter().any(|k| k.version == current);
    if !latest_installed {
        if !quiet && !keg::is_pinned(cfg, name) {
            output::opoo(&format!(
                "Skipping {name}: most recent version {current} not installed"
            ));
        }
        return vec![];
    }

    let pinned_target = std::fs::canonicalize(cfg.pinned_record(name)).ok();
    kegs.into_iter()
        .filter(|k| {
            let keg_scheme = k
                .receipt()
                .ok()
                .and_then(|r| r.source.versions.map(|v| v.version_scheme))
                .unwrap_or(0);
            if current_scheme > keg_scheme {
                true
            } else if current_scheme == keg_scheme {
                current > k.version
            } else {
                false
            }
        })
        .filter(|k| {
            if k.is_linked(cfg) {
                if !quiet {
                    output::opoo(&format!(
                        "Skipping (old) {} due to it being linked",
                        k.path.display()
                    ));
                }
                return false;
            }
            if pinned_target.as_deref() == std::fs::canonicalize(&k.path).ok().as_deref() {
                if !quiet {
                    output::opoo(&format!(
                        "Skipping (old) {} due to it being pinned",
                        k.path.display()
                    ));
                }
                return false;
            }
            true
        })
        .collect()
}

/// `Cleanup#formula_cache_paths` plus `stale_formula?`, for one formula.
fn cleanup_formula_downloads(sweeper: &mut Sweeper<'_>, index: &Index, name: &str) {
    let cfg = sweeper.cfg;
    let manifest_prefix = format!("{name}_bottle_manifest");
    for path in cache_children(cfg) {
        let Some(base) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((prefix, _)) = base.split_once("--") else {
            continue;
        };
        if prefix != name && prefix != manifest_prefix {
            continue;
        }
        if stale_download(sweeper, index, &path) {
            remove_with_link(sweeper, &path);
        }
    }
}

/// `Cleanup.stale_formula?` for a `$CACHE/<name>[_bottle_manifest]--<version>`
/// entry: stale when the API's current version is not the cached one, when the
/// cache is being scrubbed, or when the file is older than the age limit.
fn stale_download(sweeper: &Sweeper<'_>, index: &Index, path: &Path) -> bool {
    let base = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    if base.ends_with(".incomplete") {
        return true;
    }
    if sweeper.opts.scrub {
        return true;
    }
    let Some((prefix, rest)) = base.split_once("--") else {
        return is_older_than(path, sweeper.days());
    };
    let (formula, manifest) = match prefix.strip_suffix("_bottle_manifest") {
        Some(name) => (name, true),
        None => (prefix, false),
    };
    // An entry naming a formula the API does not know is left alone, exactly
    // as `stale_formula?` bails out when `Formulary.from_rack` finds nothing.
    let Some(current) = index.formula_pkg_version(formula) else {
        return is_older_than(path, sweeper.days());
    };
    let rebuild = index
        .formula(formula)
        .map(|e| e.bottle_rebuild)
        .unwrap_or(0);
    // Manifests are cached under `<version>[-<rebuild>]`, blobs under
    // `<version>.<tag>.bottle[...].tar.gz`.
    let wanted = if manifest && rebuild > 0 {
        format!("{current}-{rebuild}")
    } else {
        current.clone()
    };
    if rest != wanted && !rest.starts_with(&format!("{wanted}.")) {
        return true;
    }
    is_older_than(path, sweeper.days())
}

/// `Cleanup#cleanup_cask`: drop cached downloads that are not the current
/// version, and stage directories of versions that are gone.
fn cleanup_cask(sweeper: &mut Sweeper<'_>, index: &Index, token: &str) {
    let cfg = sweeper.cfg;
    let current = index.cask(token).and_then(|c| c.version);
    if let Some(installed) = cask::installed_cask(cfg, token) {
        let caskroom = installed.caskroom_path.clone();
        if let Ok(entries) = std::fs::read_dir(&caskroom) {
            let mut stale: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .filter(|p| {
                    let name = p
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    !name.starts_with('.') && name != installed.version
                })
                .collect();
            stale.sort();
            for path in stale {
                sweeper.remove(&path.clone(), || std::fs::remove_dir_all(&path));
            }
        }
    }
    let cask_cache = cfg.cache.join("Cask");
    let Ok(entries) = std::fs::read_dir(&cask_cache) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let Some(base) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((prefix, rest)) = base.split_once("--") else {
            continue;
        };
        if prefix != token {
            continue;
        }
        let stale = match &current {
            None => true,
            Some(v) => !(rest == *v || rest.starts_with(&format!("{v}."))),
        };
        if stale || sweeper.opts.scrub || is_older_than(&path, sweeper.days()) {
            remove_with_link(sweeper, &path);
        }
    }
}

/// Remove a cache entry: symlinks in `$CACHE` point at the real file in
/// `$CACHE/downloads`, so both go.
fn remove_with_link(sweeper: &mut Sweeper<'_>, path: &Path) {
    let target = path
        .is_symlink()
        .then(|| std::fs::canonicalize(path).ok())
        .flatten();
    let owned = path.to_path_buf();
    sweeper.remove(&owned, || std::fs::remove_file(&owned));
    if let Some(target) = target {
        sweeper.remove(&target.clone(), || std::fs::remove_file(&target));
    }
}

/// `Cleanup#cleanup_cache`: stale and `.incomplete` downloads, anything older
/// than `days`, and finally the downloads nothing in the cache references.
fn cleanup_cache(sweeper: &mut Sweeper<'_>, index: &Index) {
    let cfg = sweeper.cfg;

    for path in cache_children(cfg) {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if name == ".cleaned" || name == "api" || name == "downloads" || name == "Cask" {
            continue;
        }
        if stale_download(sweeper, index, &path) {
            remove_with_link(sweeper, &path);
        }
    }

    // `cleanup_unreferenced_downloads`: anything in `downloads` no cache
    // symlink points at.
    let downloads = cfg.cache_downloads();
    let referenced: HashSet<PathBuf> = cache_children(cfg)
        .iter()
        .chain(cache_children(&cask_cache_config(cfg)).iter())
        .filter(|p| p.is_symlink())
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect();
    let Ok(entries) = std::fs::read_dir(&downloads) else {
        return;
    };
    let mut orphans: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| !referenced.contains(&std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())))
        .collect();
    orphans.sort();
    for path in orphans {
        if sweeper.opts.dry_run {
            // `cleanup_unreferenced_downloads` returns early on a dry run.
            continue;
        }
        let owned = path.clone();
        sweeper.remove(&owned, || {
            if owned.is_dir() {
                std::fs::remove_dir_all(&owned)
            } else {
                std::fs::remove_file(&owned)
            }
        });
    }
}

/// The `Cask` subdirectory of the cache, as a `Config` for `cache_children`.
fn cask_cache_config(cfg: &Config) -> Config {
    let mut sub = cfg.clone();
    sub.cache = cfg.cache.join("Cask");
    sub
}

fn cache_children(cfg: &Config) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(&cfg.cache) else {
        return vec![];
    };
    let mut out: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    out.sort();
    out
}

/// `Cleanup#cleanup_logs`.
fn cleanup_logs(sweeper: &mut Sweeper<'_>) {
    let logs = sweeper.cfg.logs.clone();
    let Ok(entries) = std::fs::read_dir(&logs) else {
        return;
    };
    let days = sweeper.days().min(LOG_PRUNE_DAYS);
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        if !is_older_than(&dir, days) {
            continue;
        }
        let owned = dir.clone();
        sweeper.remove(&owned, || std::fs::remove_dir_all(&owned));
    }
}

/// `Cleanup#cleanup_lockfiles`: remove every lock file nothing holds.
fn cleanup_lockfiles(cfg: &Config, opts: &CleanupOptions) {
    if opts.dry_run {
        return;
    }
    let Ok(entries) = std::fs::read_dir(cfg.locks_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        else {
            continue;
        };
        use std::os::unix::io::AsRawFd;
        // SAFETY: `file` is open for the duration of the call.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if !locked {
            continue;
        }
        let _ = std::fs::remove_file(&path);
        // SAFETY: same descriptor.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// `Cleanup#prune_prefix_symlinks_and_directories`.
pub fn prune_prefix_symlinks_and_directories(cfg: &Config, opts: &CleanupOptions) {
    const MUST_EXIST: [&str; 9] = [
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
    let must_exist: Vec<PathBuf> = MUST_EXIST.iter().map(|d| cfg.prefix.join(d)).collect();
    let mut links = 0usize;
    let mut dirs_removed = 0usize;
    let mut dirs: Vec<PathBuf> = Vec::new();

    for root in &must_exist {
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(root).into_iter().flatten() {
            let path = entry.path().to_path_buf();
            if path.is_symlink() {
                if path.exists() {
                    continue;
                }
                if opts.dry_run {
                    println!("Would remove (broken link): {}", path.display());
                } else if std::fs::remove_file(&path).is_ok() {
                    links += 1;
                }
            } else if path.is_dir() && !must_exist.contains(&path) {
                dirs.push(path);
            }
        }
    }
    dirs.sort();
    for dir in dirs.iter().rev() {
        if opts.dry_run {
            if std::fs::read_dir(dir)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
            {
                println!("Would remove (empty directory): {}", dir.display());
            }
        } else if std::fs::remove_dir(dir).is_ok() {
            dirs_removed += 1;
        }
    }

    // Broken Caskroom links, then the count line.
    if let Ok(entries) = std::fs::read_dir(cfg.caskroom()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_symlink() || path.exists() {
                continue;
            }
            if opts.dry_run {
                println!("Would remove (broken link): {}", path.display());
            } else if std::fs::remove_file(&path).is_ok() {
                links += 1;
            }
        }
    }
    if opts.dry_run || (links == 0 && dirs_removed == 0) {
        return;
    }
    let mut line = format!("Pruned {links} symbolic links ");
    if dirs_removed > 0 {
        line.push_str(&format!("and {dirs_removed} directories "));
    }
    println!("{line}from {}", cfg.prefix.display());
}

/// `Cleanup.prune?`: both mtime and ctime must be older than `days`.
fn is_older_than(path: &Path, days: u64) -> bool {
    if days == 0 {
        return true;
    }
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let cutoff = match SystemTime::now().checked_sub(Duration::from_secs(days * 86_400)) {
        Some(t) => t,
        None => return false,
    };
    use std::os::unix::fs::MetadataExt;
    let ctime = SystemTime::UNIX_EPOCH + Duration::from_secs(md.ctime().max(0) as u64);
    md.modified().map(|m| m < cutoff).unwrap_or(false) && ctime < cutoff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_compares_against_the_age_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        assert!(is_older_than(&path, 0), "days: 0 means everything");
        assert!(!is_older_than(&path, 1));
        let old = SystemTime::now() - Duration::from_secs(40 * 86_400);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
        // ctime cannot be moved back, so a freshly created file is never
        // pruned by age alone, exactly like Homebrew's `prune?`.
        assert!(!is_older_than(&path, 30));
    }

    #[test]
    fn only_older_unlinked_kegs_are_eligible() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        for v in ["1.8.1", "1.8.2"] {
            std::fs::create_dir_all(cfg.cellar.join("jq").join(v)).unwrap();
        }
        // Without an index entry nothing is eligible.
        assert!(keg::installed_kegs(&cfg, "jq").len() == 2);
    }
}
