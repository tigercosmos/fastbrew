//! Installed kegs: discovery, receipts, linking, records and locks
//! (`docs/COMPAT.md` 3 and 5).

pub mod link;
pub mod lock;

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::Result;
use crate::model::FormulaReceipt;
use crate::version::PkgVersion;

/// One installed version of a formula: `$CELLAR/<name>/<version>`.
#[derive(Debug, Clone)]
pub struct Keg {
    pub name: String,
    pub version: PkgVersion,
    pub path: PathBuf,
}

impl Keg {
    pub fn new(cfg: &Config, name: &str, version: &str) -> Keg {
        Keg {
            name: name.to_string(),
            version: PkgVersion::parse(version),
            path: cfg.rack(name).join(version),
        }
    }

    pub fn receipt_path(&self) -> PathBuf {
        self.path.join("INSTALL_RECEIPT.json")
    }

    pub fn receipt(&self) -> Result<FormulaReceipt> {
        Ok(FormulaReceipt::read(&self.receipt_path())?)
    }

    /// Whether `opt/<name>` points at this keg.
    pub fn is_optlinked(&self, cfg: &Config) -> bool {
        std::fs::canonicalize(cfg.opt_record(&self.name)).ok()
            == std::fs::canonicalize(&self.path).ok()
    }

    /// Whether `var/homebrew/linked/<name>` points at this keg.
    pub fn is_linked(&self, cfg: &Config) -> bool {
        std::fs::canonicalize(cfg.linked_record(&self.name)).ok()
            == std::fs::canonicalize(&self.path).ok()
    }

    /// `(files, bytes)` for the summary line (`Pathname#abv`: counts files, sums sizes).
    pub fn disk_usage(&self) -> (u64, u64) {
        disk_usage(&self.path)
    }

    /// `<name>/<version>` used in messages.
    pub fn rel(&self) -> String {
        format!("{}/{}", self.name, self.version)
    }
}

/// `(files, bytes)` counted exactly as `DiskUsageExtension#compute_disk_usage`
/// does: every non-directory entry is a file (`.DS_Store` excluded from the
/// count), directories and directory symlinks contribute their own `lstat`
/// size, and a hardlinked inode is only counted once. `brew info`, the install
/// summary and `Uninstalling ...` all print this pair, so it has to agree with
/// Homebrew byte for byte.
pub fn disk_usage(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;

    let Ok(top) = std::fs::symlink_metadata(path) else {
        return (0, 0);
    };
    if top.file_type().is_symlink() && !path.exists() {
        return (1, 0);
    }
    if !path.is_dir() {
        return (1, std::fs::metadata(path).map(|m| m.len()).unwrap_or(0));
    }

    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut seen: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    // `Pathname#find` yields the root itself as well, and measures with `lstat`,
    // so a symlink counts its own size rather than its target's.
    for entry in walkdir::WalkDir::new(path).into_iter().flatten() {
        let Ok(md) = entry.path().symlink_metadata() else {
            continue;
        };
        let ft = md.file_type();
        if ft.is_dir() || (ft.is_symlink() && entry.path().is_dir()) {
            bytes += md.len();
            continue;
        }
        if entry.file_name() != std::ffi::OsStr::new(".DS_Store") {
            files += 1;
        }
        if seen.insert((md.dev(), md.ino())) {
            bytes += md.len();
        }
    }
    (files, bytes)
}

/// All installed kegs of `name`, sorted by version ascending.
pub fn installed_kegs(cfg: &Config, name: &str) -> Vec<Keg> {
    let rack = cfg.rack(name);
    let Ok(rd) = std::fs::read_dir(&rack) else {
        return vec![];
    };
    let mut kegs: Vec<Keg> = rd
        .flatten()
        .filter(|e| {
            e.file_type()
                .map(|t| t.is_dir() || t.is_symlink())
                .unwrap_or(false)
        })
        .filter_map(|e| {
            let fname = e.file_name();
            let v = fname.to_str()?;
            if v.starts_with('.') || !e.path().is_dir() {
                return None;
            }
            Some(Keg {
                name: name.to_string(),
                version: PkgVersion::parse(v),
                path: e.path(),
            })
        })
        .collect();
    kegs.sort_by(|a, b| a.version.cmp(&b.version));
    kegs
}

/// Highest-versioned installed keg.
pub fn latest_keg(cfg: &Config, name: &str) -> Option<Keg> {
    installed_kegs(cfg, name)
        .into_iter()
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// Names of all racks in the Cellar that contain at least one keg, sorted.
pub fn installed_formula_names(cfg: &Config) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(&cfg.cellar) else {
        return vec![];
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if name.starts_with('.') || !e.path().is_dir() {
                return None;
            }
            let has_keg = std::fs::read_dir(e.path())
                .map(|r| {
                    r.flatten().any(|k| {
                        k.path().is_dir() && !k.file_name().to_string_lossy().starts_with('.')
                    })
                })
                .unwrap_or(false);
            has_keg.then_some(name)
        })
        .collect();
    names.sort();
    names
}

/// Keg currently linked (via `var/homebrew/linked`), falling back to the opt link, for `name`.
pub fn linked_keg(cfg: &Config, name: &str) -> Option<Keg> {
    [cfg.linked_record(name), cfg.opt_record(name)]
        .into_iter()
        .find_map(|record| keg_at_record(cfg, &record))
}

/// `NamedArgs#resolve_default_keg`: the keg a command that names a rack without
/// a version operates on — the opt-linked keg, then the linked one, then the
/// only installed keg, and only then the newest.
///
/// `unlink` resolves this way, so it removes the links that are actually there
/// rather than those of whichever keg happens to sort highest.
pub fn default_keg(cfg: &Config, name: &str) -> Option<Keg> {
    if let Some(keg) = [cfg.opt_record(name), cfg.linked_record(name)]
        .into_iter()
        .find_map(|record| keg_at_record(cfg, &record))
    {
        return Some(keg);
    }
    let kegs = installed_kegs(cfg, name);
    if kegs.len() == 1 {
        return kegs.into_iter().next();
    }
    kegs.into_iter().max_by(|a, b| a.version.cmp(&b.version))
}

/// The keg a prefix record (`opt/<name>`, `var/homebrew/linked/<name>`) points
/// at, or `None` when it is absent, broken or points outside the Cellar.
fn keg_at_record(cfg: &Config, record: &std::path::Path) -> Option<Keg> {
    if !record.is_symlink() {
        return None;
    }
    let target = std::fs::canonicalize(record).ok()?;
    let version = target.file_name()?.to_str()?.to_string();
    let rack_name = target.parent()?.file_name()?.to_str()?.to_string();
    if target.parent()?.parent()? != std::fs::canonicalize(&cfg.cellar).ok()?.as_path() {
        return None;
    }
    // The keg path is spelled the way the configured cellar is, not the way
    // `realpath` would (`Utils::Path.resolved_path` expands the link but not
    // its ancestors): every other path in the prefix is built that way, and
    // the link bookkeeping compares them literally.
    Some(Keg {
        name: rack_name.clone(),
        version: PkgVersion::parse(&version),
        path: cfg.cellar.join(rack_name).join(version),
    })
}

pub fn is_pinned(cfg: &Config, name: &str) -> bool {
    cfg.pinned_record(name).is_symlink()
}

/// Human readable size like Homebrew's `disk_usage_readable` (`1.2MB`,
/// `186KB`, `64.9MB`, `8B`): one decimal place unless it would be a trailing
/// zero (`Formatter.disk_usage_readable` in `utils/formatter.rb`).
pub fn disk_usage_readable(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < units.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if ((value * 10.0) as i64) % 10 == 0 {
        format!("{}{}", value as i64, units[unit])
    } else {
        format!("{value:.1}{}", units[unit])
    }
}

/// Write `data` to `path` via a temp file in the same directory and rename.
///
/// `Pathname#atomic_write` keeps an existing file's mode and gives a new one
/// `0666 & ~umask` (0644 for a normal umask); the temp file it is built from
/// would otherwise be 0600, which would leave receipts unreadable to everyone
/// but the installing user.
pub fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let existing_mode = std::fs::metadata(path).ok().map(|m| m.permissions().mode());
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, data)?;
    let mode = existing_mode.unwrap_or(0o666 & !umask());
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// The process umask, read without changing it for longer than the call takes.
fn umask() -> u32 {
    // SAFETY: `umask` cannot fail; it is restored immediately. Two threads
    // racing here would both end up restoring the same original value.
    unsafe {
        let current = libc::umask(0o022);
        libc::umask(current);
        current as u32
    }
}

/// Create `dst` as a symlink to `src` expressed relative to `dst`'s directory
/// (`Pathname#make_relative_symlink`). Neither path is canonicalized.
pub fn make_relative_symlink(dst: &Path, src: &Path) -> std::io::Result<()> {
    let target = relative_path(src, dst.parent().unwrap_or_else(|| Path::new("/")));
    std::os::unix::fs::symlink(target, dst)
}

/// `src` relative to directory `base` (both absolute; lexical, like `Pathname#relative_path_from`).
pub fn relative_path(src: &Path, base: &Path) -> PathBuf {
    let s: Vec<_> = src.components().collect();
    let b: Vec<_> = base.components().collect();
    let common = s.iter().zip(b.iter()).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..b.len() {
        out.push("..");
    }
    for c in &s[common..] {
        out.push(c);
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        assert_eq!(
            relative_path(Path::new("/p/Cellar/tree/2.3.2"), Path::new("/p/opt")),
            PathBuf::from("../Cellar/tree/2.3.2")
        );
        assert_eq!(
            relative_path(
                Path::new("/p/Cellar/tree/2.3.2"),
                Path::new("/p/var/homebrew/linked")
            ),
            PathBuf::from("../../../Cellar/tree/2.3.2")
        );
        assert_eq!(
            relative_path(
                Path::new("/p/Cellar/tree/2.3.2/bin/tree"),
                Path::new("/p/bin")
            ),
            PathBuf::from("../Cellar/tree/2.3.2/bin/tree")
        );
    }

    #[test]
    fn readable_sizes() {
        assert_eq!(disk_usage_readable(8), "8B");
        assert_eq!(disk_usage_readable(186_010), "186KB");
        assert_eq!(disk_usage_readable(1_235_416), "1.2MB");
    }
}
