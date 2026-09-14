//! Outdated detection for formulae and casks (`docs/COMPAT.md` 8).
//!
//! Port of `Formula#outdated_kegs` and `Cask::Cask#outdated_version`.

use crate::api::index::Index;
use crate::config::Config;
use crate::error::Result;
use crate::keg::Keg;
use crate::model::FormulaEntry;
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
    let Some(i) = index.formula_index(name) else {
        // Not in the API any more: nothing to upgrade to.
        return vec![];
    };
    outdated_kegs_against(
        cfg,
        name,
        index.formula_pkg_version_at(i),
        index.formula_version_scheme_at(i),
    )
}

/// `Formula#outdated_kegs` for a *resolved* entry, which is the only way to
/// judge a formula installed from a tap: the core index either does not know
/// the name or knows a different formula by it.
pub fn outdated_kegs_for(cfg: &Config, entry: &FormulaEntry) -> Vec<Keg> {
    outdated_kegs_against(cfg, &entry.name, &entry.pkg_version(), entry.version_scheme)
}

fn outdated_kegs_against(cfg: &Config, name: &str, latest: &str, latest_scheme: u32) -> Vec<Keg> {
    let kegs = crate::keg::installed_kegs(cfg, name);
    if kegs.is_empty() {
        return vec![];
    }
    let latest_pkg_version = PkgVersion::parse(latest);
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

/// Outdated formulae among `names`, or among everything installed.
///
/// Each rack is resolved through [`crate::resolve::resolve_installed`], so a
/// keg poured from a third-party tap is compared against that tap's formula
/// and not against a core formula that happens to share the name.
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
        // The core index answers for almost every rack without touching a
        // tap; only a keg whose receipt names a third-party tap pays for the
        // tap lookup. A rack the API no longer knows has nothing to upgrade to.
        let entry = match crate::resolve::installed_tap(cfg, &name) {
            Some(tap) => {
                match crate::resolve::resolve_formula(cfg, index, &format!("{tap}/{name}")) {
                    Ok(entry) => entry,
                    Err(_) => continue,
                }
            }
            None => match index.formula(&name) {
                Some(entry) => entry,
                None => continue,
            },
        };
        let kegs = outdated_kegs_for(cfg, &entry);
        if kegs.is_empty() {
            continue;
        }
        let current_version = entry.pkg_version();
        if current_version.is_empty() {
            continue;
        }
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
    // Homebrew derives the installed version from the metadata caskfile
    // (`Caskroom.cask_installed_version`); `cask::installed_cask` does the
    // same and ignores `<version>.upgrading` rollback backups.
    if let Some(installed) = crate::cask::installed_cask(cfg, token) {
        return Some(installed.version);
    }
    // Fallback for a Caskroom without metadata: the newest staged version
    // directory, never an upgrade backup.
    let dir = cfg.caskroom().join(token);
    let mut versions: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if name.starts_with('.') || name.ends_with(crate::cask::BACKUP_SUFFIX) {
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
    for requested in candidates {
        // A tap-qualified request (`user/repo/token`) names the Caskroom entry
        // by its bare token.
        let token = requested
            .rsplit('/')
            .next()
            .unwrap_or(&requested)
            .to_string();
        let Some(installed_version) = installed_cask_version(cfg, &token) else {
            continue;
        };
        // Core casks come straight from the index; a cask installed from a
        // tap is resolved through the tap recorded in its receipt, like
        // `Caskroom.casks` loading each installed cask.
        let (current, is_latest, auto_updates) = match index.cask_index(&token) {
            Some(i) if !requested.contains('/') => (
                index.cask_version_at(i).to_string(),
                index.cask_is_latest_at(i),
                index.cask_auto_updates_at(i),
            ),
            _ => {
                let reference = if requested.contains('/') {
                    requested.clone()
                } else {
                    crate::deps::installed_cask_reference(cfg, &token)
                };
                match crate::resolve::resolve_cask(cfg, index, &reference) {
                    Ok(entry) => (
                        entry.version.clone().unwrap_or_default(),
                        entry.is_latest(),
                        entry.auto_updates,
                    ),
                    Err(_) => continue,
                }
            }
        };
        if current.is_empty() {
            continue;
        }
        if is_latest {
            // `version :latest` casks only count with `--greedy`; Homebrew then
            // compares the download sha, which fastbrew cannot do offline.
            if !greedy {
                continue;
            }
        } else {
            // An `auto_updates` cask updates itself, so it is only reported
            // with `--greedy`.
            let same = installed_version == current;
            if same || (auto_updates && !greedy) {
                continue;
            }
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
