//! `update` and the auto-update policy (`docs/DESIGN.md` 9).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::api::fetch::{self, FetchOutcome};
use crate::api::index::Index;
use crate::config::Config;
use crate::error::Result;
use crate::output;
use crate::platform::Host;

#[derive(Debug, Clone, Default)]
pub struct UpdateReport {
    pub api_updated: bool,
    pub taps_updated: Vec<String>,
    pub new_formulae: Vec<String>,
    pub updated_formulae: Vec<String>,
    pub renamed_formulae: Vec<(String, String)>,
    pub deleted_formulae: Vec<String>,
    pub new_casks: Vec<String>,
    pub updated_casks: Vec<String>,
    pub deleted_casks: Vec<String>,
    pub outdated_formulae: Vec<String>,
    pub outdated_casks: Vec<String>,
}

impl UpdateReport {
    pub fn is_empty(&self) -> bool {
        self.new_formulae.is_empty()
            && self.updated_formulae.is_empty()
            && self.renamed_formulae.is_empty()
            && self.deleted_formulae.is_empty()
            && self.new_casks.is_empty()
            && self.updated_casks.is_empty()
            && self.deleted_casks.is_empty()
            && self.taps_updated.is_empty()
    }
}

/// Name -> version snapshot used to diff two index generations.
#[derive(Default)]
struct Snapshot {
    formulae: BTreeMap<String, String>,
    casks: BTreeMap<String, String>,
    renames: BTreeMap<String, String>,
}

fn snapshot(index: &Index) -> Snapshot {
    let formulae = (0..index.formula_count())
        .map(|i| {
            (
                index.formula_name_at(i).to_string(),
                index.formula_pkg_version_at(i).to_string(),
            )
        })
        .collect();
    let casks = (0..index.cask_count())
        .map(|i| {
            (
                index.cask_token_at(i).to_string(),
                index.cask_version_at(i).to_string(),
            )
        })
        .collect();
    Snapshot {
        formulae,
        casks,
        renames: index.formula_renames(),
    }
}

fn diff(old: &Snapshot, new: &Snapshot, report: &mut UpdateReport) {
    for (name, version) in &new.formulae {
        match old.formulae.get(name) {
            None => report.new_formulae.push(name.clone()),
            Some(old_version) if old_version != version => {
                report.updated_formulae.push(name.clone())
            }
            _ => {}
        }
    }
    for name in old.formulae.keys() {
        if new.formulae.contains_key(name) {
            continue;
        }
        match new.renames.get(name) {
            Some(to) if !old.renames.contains_key(name) => {
                report.renamed_formulae.push((name.clone(), to.clone()))
            }
            _ => report.deleted_formulae.push(name.clone()),
        }
    }
    for (token, version) in &new.casks {
        match old.casks.get(token) {
            None => report.new_casks.push(token.clone()),
            Some(old_version) if old_version != version => report.updated_casks.push(token.clone()),
            _ => {}
        }
    }
    for token in old.casks.keys() {
        if !new.casks.contains_key(token) {
            report.deleted_casks.push(token.clone());
        }
    }
}

/// Marker whose mtime records the last auto-update check.
fn last_check_marker(cfg: &Config) -> PathBuf {
    cfg.cache_fastbrew().join(".last_auto_update")
}

fn age(path: &PathBuf) -> Option<Duration> {
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(mtime).ok()
}

pub fn update(cfg: &Config, force: bool, quiet: bool, auto: bool) -> Result<UpdateReport> {
    let tag = Host::detect().bottle_tag();
    let mut report = UpdateReport::default();

    // Snapshot the current generation before the file is replaced.
    let before = Index::load(cfg, &tag).ok().map(|i| snapshot(&i));

    let stale = if force {
        None
    } else if auto {
        Some(cfg.auto_update_secs)
    } else {
        None
    };
    let outcome = fetch::fetch_packages(cfg, &tag, stale, quiet)?;
    report.api_updated = outcome == FetchOutcome::Updated;

    // Rebuild (or reuse) the fast index for the current file.
    let index = if report.api_updated {
        Index::rebuild(cfg, &tag)?
    } else {
        Index::load(cfg, &tag)?
    };

    if let Some(before) = before
        && report.api_updated
    {
        diff(&before, &snapshot(&index), &mut report);
    }

    // TODO(phase2): tap::update_all(cfg, quiet) to fetch and fast-forward
    // third-party taps and fill in `report.taps_updated`.

    report.outdated_formulae = crate::ops::outdated::outdated_formulae(cfg, &index, None)?
        .into_iter()
        .map(|o| o.name)
        .collect();
    report.outdated_casks = crate::ops::outdated::outdated_casks(cfg, &index, None, false)?
        .into_iter()
        .map(|o| o.token)
        .collect();

    let _ = std::fs::create_dir_all(cfg.cache_fastbrew());
    let _ = std::fs::write(last_check_marker(cfg), b"");
    Ok(report)
}

/// Run the auto-update if policy says so; never fails the calling command.
pub fn auto_update_if_needed(cfg: &Config, command: &str) {
    if cfg.no_auto_update {
        return;
    }
    if !matches!(command, "install" | "upgrade" | "outdated" | "tap") {
        return;
    }
    // Only check again after `HOMEBREW_API_AUTO_UPDATE_SECS`.
    if let Some(a) = age(&last_check_marker(cfg))
        && a < Duration::from_secs(cfg.api_auto_update_secs)
    {
        return;
    }
    // Only refresh when the cached API file is older than
    // `HOMEBREW_AUTO_UPDATE_SECS`.
    let tag = Host::detect().bottle_tag();
    let packages = fetch::packages_path(cfg, &tag);
    if let Some(a) = age(&packages)
        && a < Duration::from_secs(cfg.auto_update_secs)
    {
        return;
    }
    let quiet = std::env::var_os("HOMEBREW_AUTO_UPDATE_QUIET").is_some_and(|v| !v.is_empty());
    match update(cfg, false, true, true) {
        Ok(report) if !quiet && !report.is_empty() => print_report(cfg, &report, true),
        _ => {}
    }
}

fn print_section(title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    output::ohai(title);
    output::print_columns(items);
}

pub fn print_report(cfg: &Config, report: &UpdateReport, quiet: bool) {
    let _ = cfg;
    if !report.taps_updated.is_empty() {
        println!(
            "Updated {}.",
            output::plural(report.taps_updated.len() as u64, "tap")
        );
    }
    if report.is_empty() {
        if !quiet {
            println!("Already up-to-date.");
        }
        return;
    }

    print_section("New Formulae", &report.new_formulae);
    if !quiet {
        print_section("Updated Formulae", &report.updated_formulae);
    } else if !report.updated_formulae.is_empty() {
        println!(
            "==> Updated {}.",
            output::plural(report.updated_formulae.len() as u64, "Formula")
        );
    }
    if !report.renamed_formulae.is_empty() {
        output::ohai("Renamed Formulae");
        for (from, to) in &report.renamed_formulae {
            println!("{from} -> {to}");
        }
    }
    print_section("Deleted Formulae", &report.deleted_formulae);
    print_section("New Casks", &report.new_casks);
    if !quiet {
        print_section("Updated Casks", &report.updated_casks);
    } else if !report.updated_casks.is_empty() {
        println!(
            "==> Updated {}.",
            output::plural(report.updated_casks.len() as u64, "Cask")
        );
    }
    print_section("Deleted Casks", &report.deleted_casks);
    print_section("Outdated Formulae", &report.outdated_formulae);
    print_section("Outdated Casks", &report.outdated_casks);
    if !report.outdated_formulae.is_empty() || !report.outdated_casks.is_empty() {
        println!(
            "\nYou have {} and {} installed.\nYou can upgrade them with `fastbrew upgrade`\nor list them with `fastbrew outdated`.",
            output::plural(report.outdated_formulae.len() as u64, "outdated formula"),
            output::plural(report.outdated_casks.len() as u64, "outdated cask"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(f: &[(&str, &str)], c: &[(&str, &str)], r: &[(&str, &str)]) -> Snapshot {
        Snapshot {
            formulae: f
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            casks: c
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            renames: r
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        }
    }

    #[test]
    fn diffs_generations() {
        let old = snap(
            &[("jq", "1.8.1"), ("gone", "1"), ("old", "2")],
            &[("firefox", "1"), ("dead", "3")],
            &[],
        );
        let new = snap(
            &[("jq", "1.8.2"), ("new", "1")],
            &[("firefox", "2"), ("brave", "1")],
            &[("old", "renamed")],
        );
        let mut report = UpdateReport::default();
        diff(&old, &new, &mut report);
        assert_eq!(report.new_formulae, vec!["new"]);
        assert_eq!(report.updated_formulae, vec!["jq"]);
        assert_eq!(report.deleted_formulae, vec!["gone"]);
        assert_eq!(
            report.renamed_formulae,
            vec![("old".to_string(), "renamed".to_string())]
        );
        assert_eq!(report.new_casks, vec!["brave"]);
        assert_eq!(report.updated_casks, vec!["firefox"]);
        assert_eq!(report.deleted_casks, vec!["dead"]);
    }
}
