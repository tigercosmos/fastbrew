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

/// `(files, bytes)`: regular files and symlinks (not directories) under `path`, sizes of regular files.
pub fn disk_usage(path: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    for entry in walkdir::WalkDir::new(path).into_iter().flatten() {
        let ft = entry.file_type();
        if ft.is_dir() {
            continue;
        }
        files += 1;
        if ft.is_file()
            && let Ok(md) = entry.metadata()
        {
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
    for record in [cfg.linked_record(name), cfg.opt_record(name)] {
        if !record.is_symlink() {
            continue;
        }
        let Ok(target) = std::fs::canonicalize(&record) else {
            continue;
        };
        let version = target.file_name()?.to_str()?.to_string();
        let rack_name = target.parent()?.file_name()?.to_str()?.to_string();
        if target.parent()?.parent()? != std::fs::canonicalize(&cfg.cellar).ok()?.as_path() {
            continue;
        }
        return Some(Keg {
            name: rack_name,
            version: PkgVersion::parse(&version),
            path: target,
        });
    }
    None
}

pub fn is_pinned(cfg: &Config, name: &str) -> bool {
    cfg.pinned_record(name).is_symlink()
}

/// Human readable size like Homebrew's `disk_usage_readable` (`1.2MB`, `186KB`, `8B`).
pub fn disk_usage_readable(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < units.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else if value >= 10.0 {
        format!("{value:.0}{}", units[unit])
    } else {
        format!("{value:.1}{}", units[unit])
    }
}

/// Write `data` to `path` via a temp file in the same directory and rename.
pub fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, data)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
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
