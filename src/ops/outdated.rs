//! Outdated detection for formulae and casks (`docs/COMPAT.md` 8).
//!
//! Port of `Formula#outdated_kegs` and `Cask::Cask#outdated_version`.

use crate::api::index::Index;
use crate::config::Config;
use crate::error::Result;
use crate::keg::Keg;
use crate::version::PkgVersion;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutdatedFormula {
    pub name: String,
    pub installed_versions: Vec<String>,
    pub current_version: String,
    pub pinned: bool,
    pub pinned_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutdatedCask {
    pub token: String,
    pub installed_version: String,
    pub current_version: String,
}

/// `version_scheme` recorded in a keg's receipt (0 when absent).
fn keg_version_scheme(keg: &Keg) -> u32 {
    keg.receipt()
        .ok()
        .and_then(|r| r.source.versions.map(|v| v.version_scheme))
        .unwrap_or(0)
}

/// The pinned keg version, if `name` is pinned.
pub fn pinned_version(cfg: &Config, name: &str) -> Option<String> {
    let record = cfg.pinned_record(name);
    if !record.is_symlink() {
        return None;
    }
    let target = std::fs::read_link(&record).ok()?;
    Some(target.file_name()?.to_str()?.to_string())
}

/// Port of `Formula#outdated_kegs`: the installed kegs that make `name`
/// outdated, or an empty vector when it is current.
pub fn outdated_kegs(cfg: &Config, index: &Index, name: &str) -> Vec<Keg> {
    let kegs = crate::keg::installed_kegs(cfg, name);
    if kegs.is_empty() {
        return vec![];
    }
    let Some(i) = index.formula_index(name) else {
        // Not in the API any more: nothing to upgrade to.
        return vec![];
    };
    let latest_pkg_version = PkgVersion::parse(index.formula_pkg_version_at(i));
    let latest_scheme = index.formula_version_scheme_at(i);
    if latest_pkg_version.version.as_str().is_empty() {
        return vec![];
    }
    let pinned = crate::keg::is_pinned(cfg, name);

    let mut current = false;
    for keg in &kegs {
        if keg.version.is_head() {
            continue;
        }
        let keg_scheme = keg_version_scheme(keg);
        if latest_scheme > keg_scheme && latest_pkg_version != keg.version {
            continue;
        }
        if latest_scheme == keg_scheme && latest_pkg_version > keg.version {
            continue;
        }
        if !keg.is_optlinked(cfg) && !keg.is_linked(cfg) && !pinned {
            continue;
        }
        current = true;
        break;
    }
    if current {
        return vec![];
    }
    let mut all = kegs;
    all.sort_by(|a, b| {
        keg_version_scheme(a)
            .cmp(&keg_version_scheme(b))
            .then(a.version.cmp(&b.version))
    });
    all
}

pub fn outdated_formulae(
    cfg: &Config,
    index: &Index,
    names: Option<&[String]>,
) -> Result<Vec<OutdatedFormula>> {
    let candidates: Vec<String> = match names {
        Some(n) if !n.is_empty() => n.to_vec(),
        _ => crate::keg::installed_formula_names(cfg),
    };
    let mut out = Vec::new();
    for name in candidates {
        let kegs = outdated_kegs(cfg, index, &name);
        if kegs.is_empty() {
            continue;
        }
        let Some(current_version) = index.formula_pkg_version(&name) else {
            continue;
        };
        out.push(OutdatedFormula {
            installed_versions: kegs.iter().map(|k| k.version.to_string()).collect(),
            current_version,
            pinned: crate::keg::is_pinned(cfg, &name),
            pinned_version: pinned_version(cfg, &name),
            name,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Version staged in `Caskroom/<token>` (the newest non-dot subdirectory).
pub fn installed_cask_version(cfg: &Config, token: &str) -> Option<String> {
    let dir = cfg.caskroom().join(token);
    let mut versions: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if name.starts_with('.') {
                return None;
            }
            let mtime = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::UNIX_EPOCH);
            Some((mtime, name))
        })
        .collect();
    versions.sort();
    versions.pop().map(|(_, v)| v)
}

pub fn outdated_casks(
    cfg: &Config,
    index: &Index,
    names: Option<&[String]>,
    greedy: bool,
) -> Result<Vec<OutdatedCask>> {
    let candidates: Vec<String> = match names {
        Some(n) if !n.is_empty() => n.to_vec(),
        _ => crate::deps::installed_cask_tokens(cfg),
    };
    let mut out = Vec::new();
    for token in candidates {
        let Some(installed_version) = installed_cask_version(cfg, &token) else {
            continue;
        };
        let Some(i) = index.cask_index(&token) else {
            continue;
        };
        let current = index.cask_version_at(i).to_string();
        if current.is_empty() {
            continue;
        }
        if index.cask_is_latest_at(i) {
            // `version :latest` casks only count with `--greedy`; Homebrew then
            // compares the download sha, which fastbrew cannot do offline.
            if !greedy {
                continue;
            }
        } else if installed_version == current {
            continue;
        } else if index.cask_auto_updates_at(i) && !greedy {
            continue;
        }
        out.push(OutdatedCask {
            token,
            installed_version,
            current_version: current,
        });
    }
    out.sort_by(|a, b| a.token.cmp(&b.token));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_version_reads_the_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::from_env().unwrap();
        cfg.prefix = dir.path().to_path_buf();
        std::fs::create_dir_all(cfg.pinned_kegs()).unwrap();
        std::os::unix::fs::symlink("../../../Cellar/jq/1.8.2", cfg.pinned_record("jq")).unwrap();
        assert_eq!(pinned_version(&cfg, "jq").as_deref(), Some("1.8.2"));
        assert_eq!(pinned_version(&cfg, "wget"), None);
    }
}
