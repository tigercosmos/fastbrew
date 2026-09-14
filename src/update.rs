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
    pub auto_update: bool,
    /// Tap names whose `HEAD` moved, plus `homebrew/core`/`homebrew/cask`
    /// when the API file changed (`updated_taps` in `cmd/update-report.rb`).
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
    /// Taps whose fetch failed (`HOMEBREW_UPDATE_FAILED`).
    pub tap_failures: Vec<String>,
    /// One-line descriptions for the `New Formulae` and `New Casks` sections.
    pub descriptions: BTreeMap<String, String>,
}

impl UpdateReport {
    /// `ReporterHub#empty?`: nothing to report about formulae or casks.
    pub fn is_empty(&self) -> bool {
        self.new_formulae.is_empty()
            && self.updated_formulae.is_empty()
            && self.renamed_formulae.is_empty()
            && self.deleted_formulae.is_empty()
            && self.new_casks.is_empty()
            && self.updated_casks.is_empty()
            && self.deleted_casks.is_empty()
    }

    /// Whether anything at all moved (`updated` in `output_update_report`).
    pub fn updated(&self) -> bool {
        !self.taps_updated.is_empty()
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

/// `full_name -> pkg_version` for every parsed third-party tap formula.
fn tap_snapshot(cfg: &Config, tag: &crate::platform::BottleTag) -> BTreeMap<String, String> {
    crate::api::taps::TapIndex::load(cfg, tag)
        .all_formulae()
        .into_iter()
        .map(|f| (f.full_name(), f.pkg_version()))
        .collect()
}

pub fn update(cfg: &Config, force: bool, quiet: bool, auto: bool) -> Result<UpdateReport> {
    let tag = Host::detect().bottle_tag();
    let mut report = UpdateReport {
        auto_update: auto,
        ..Default::default()
    };

    // Snapshot both generations before anything is replaced.
    let before = Index::load(cfg, &tag).ok().map(|i| snapshot(&i));
    let taps_before = tap_snapshot(cfg, &tag);

    // `API.fetch_api_files!`: an auto-update accepts a file younger than
    // `HOMEBREW_API_AUTO_UPDATE_SECS`; an explicit `update` always revalidates.
    let stale = (!force && auto).then_some(cfg.api_auto_update_secs);

    // The API fetch and the tap pulls are independent; run them together.
    let (api, taps) = rayon::join(
        || fetch::fetch_packages(cfg, &tag, stale, quiet),
        || crate::tap::update_all(cfg, quiet),
    );
    report.tap_failures = taps.failures;
    let moved_taps = taps.changed;
    report.api_updated = api? == FetchOutcome::Updated;

    // Rebuild (or reuse) the fast index for the current file.
    let index = if report.api_updated {
        Index::rebuild(cfg, &tag)?
    } else {
        Index::load(cfg, &tag)?
    };

    if report.api_updated {
        report.taps_updated.push("homebrew/core".to_string());
        report.taps_updated.push("homebrew/cask".to_string());
        if let Some(before) = before {
            diff(&before, &snapshot(&index), &mut report);
        }
    }
    for t in &moved_taps {
        report.taps_updated.push(t.name());
        // Re-reading the tap picks up the files the pull changed.
        let after = tap_snapshot(cfg, &tag);
        for (name, version) in &after {
            match taps_before.get(name) {
                None => report.new_formulae.push(name.clone()),
                Some(old) if old != version => report.updated_formulae.push(name.clone()),
                _ => {}
            }
        }
        for name in taps_before.keys() {
            if !after.contains_key(name) {
                report.deleted_formulae.push(name.clone());
            }
        }
    }
    report.taps_updated.sort();
    report.taps_updated.dedup();
    report.new_formulae.sort();
    report.new_formulae.dedup();
    report.updated_formulae.sort();
    report.updated_formulae.dedup();
    report.deleted_formulae.sort();
    report.deleted_formulae.dedup();

    // `dump_new_formula_report` prints `name: desc` for each new name.
    for name in report.new_formulae.iter().chain(report.new_casks.iter()) {
        let desc = index
            .formula_desc(name)
            .or_else(|| index.cask_desc(name))
            .filter(|d| !d.is_empty());
        if let Some(d) = desc {
            report.descriptions.insert(name.clone(), d);
        }
    }

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

/// `HOMEBREW_AUTO_UPDATE_SECS`'s default: 5 minutes when a third-party tap
/// reference is on the command line (its metadata is not in the API), else
/// 24 hours (`utils/auto-update.sh`, `env_config.rb`).
fn auto_update_secs(cfg: &Config, args: &[String]) -> u64 {
    if std::env::var_os("HOMEBREW_AUTO_UPDATE_SECS").is_some_and(|v| !v.is_empty()) {
        return cfg.auto_update_secs;
    }
    let tap_arg = args
        .iter()
        .any(|a| a.matches('/').count() == 2 && !a.to_lowercase().starts_with("homebrew/"));
    if tap_arg { 300 } else { cfg.auto_update_secs }
}

/// Run the auto-update if policy says so; never fails the calling command.
///
/// `args` are the command's named arguments, which decide the staleness
/// window (`AUTO_UPDATE_TAP_COMMANDS` in `utils/auto-update.sh`).
pub fn auto_update_if_needed(cfg: &Config, command: &str, args: &[String]) {
    if cfg.no_auto_update {
        return;
    }
    // `setup-auto-update`: `tap` only auto-updates when given a tap name.
    let wanted = match command {
        "install" | "upgrade" | "outdated" => true,
        "tap" => !args.is_empty(),
        _ => false,
    };
    if !wanted {
        return;
    }
    // The marker stands in for the repositories' `FETCH_HEAD` mtimes: skip
    // when everything was checked within `HOMEBREW_AUTO_UPDATE_SECS`.
    if let Some(a) = age(&last_check_marker(cfg))
        && a < Duration::from_secs(auto_update_secs(cfg, args))
    {
        return;
    }
    let quiet = std::env::var_os("HOMEBREW_AUTO_UPDATE_QUIET").is_some_and(|v| !v.is_empty());
    // `HOMEBREW_AUTO_UPDATE_SKIP_OUTDATED`: a bare `upgrade`/`outdated` lists
    // the outdated packages itself, so the report must not repeat them.
    let skip_outdated = matches!(command, "upgrade" | "outdated") && args.is_empty();
    match update(cfg, false, true, true) {
        Ok(mut report) if !quiet && !report.is_empty() => {
            if skip_outdated {
                report.outdated_formulae.clear();
                report.outdated_casks.clear();
            }
            output::ohai("Auto-updated Homebrew!");
            print_report(cfg, &report, true);
            println!();
        }
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

/// `Utils::Text.to_sentence`: `a`, `a and b`, `a, b and c`.
fn to_sentence(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        _ => {
            let (last, rest) = items.split_last().expect("non-empty");
            format!("{} and {last}", rest.join(", "))
        }
    }
}

/// `Homebrew::Cmd::UpdateReport#output_update_report` plus `ReporterHub#dump`.
///
/// Homebrew 6 no longer prints "Updated Formulae", "Renamed Formulae" or
/// plain "Deleted Formulae" sections: only new packages, deleted packages
/// that are installed, and the outdated summary.
pub fn print_report(cfg: &Config, report: &UpdateReport, quiet: bool) {
    // `update.sh`: `onoe <"${update_failed_file}"` and
    // `Homebrew.failed = true if ENV["HOMEBREW_UPDATE_FAILED"]`. A failed tap
    // also suppresses the `Already up-to-date.` line below.
    for failure in &report.tap_failures {
        output::ofail(failure);
    }
    if !report.taps_updated.is_empty() {
        println!(
            "Updated {} ({}).",
            output::plural(report.taps_updated.len() as u64, "tap"),
            to_sentence(&report.taps_updated)
        );
    }
    if !report.updated() {
        if !quiet && !report.auto_update && report.tap_failures.is_empty() {
            println!("Already up-to-date.");
        }
        return;
    }
    if report.is_empty() {
        if !quiet {
            println!("No changes to formulae or casks.");
        }
        return;
    }
    if quiet {
        return;
    }

    let described = |names: &[String]| -> Vec<String> {
        names
            .iter()
            .map(|n| match report.descriptions.get(n) {
                Some(d) => format!("{n}: {d}"),
                None => n.clone(),
            })
            .collect()
    };
    // `dump_new_formula_report` drops names that are already installed.
    let new_formulae: Vec<String> = report
        .new_formulae
        .iter()
        .filter(|n| !cfg.rack(crate::deps::short_name(n)).is_dir())
        .cloned()
        .collect();
    if !new_formulae.is_empty() {
        output::ohai("New Formulae");
        for line in described(&new_formulae) {
            println!("{line}");
        }
    }
    let any_casks = crate::deps::installed_cask_tokens(cfg).is_empty();
    if !any_casks {
        let new_casks: Vec<String> = report
            .new_casks
            .iter()
            .filter(|t| !cfg.caskroom().join(t).is_dir())
            .cloned()
            .collect();
        if !new_casks.is_empty() {
            output::ohai("New Casks");
            for line in described(&new_casks) {
                println!("{line}");
            }
        }
    }
    let deleted_formulae: Vec<String> = report
        .deleted_formulae
        .iter()
        .filter(|n| cfg.rack(crate::deps::short_name(n)).is_dir())
        .cloned()
        .collect();
    print_section("Deleted Installed Formulae", &deleted_formulae);
    let deleted_casks: Vec<String> = report
        .deleted_casks
        .iter()
        .filter(|t| cfg.caskroom().join(t).is_dir())
        .cloned()
        .collect();
    print_section("Deleted Installed Casks", &deleted_casks);

    if !report.auto_update {
        print_section("Outdated Formulae", &report.outdated_formulae);
        print_section("Outdated Casks", &report.outdated_casks);
    }
    let (formulae, casks) = (report.outdated_formulae.len(), report.outdated_casks.len());
    if formulae == 0 && casks == 0 {
        return;
    }
    let mut msg = String::new();
    if formulae > 0 {
        msg.push_str(&format!(
            "{} outdated {}",
            output::bold(&formulae.to_string()),
            if formulae == 1 { "formula" } else { "formulae" }
        ));
    }
    if casks > 0 {
        if !msg.is_empty() {
            msg.push_str(" and ");
        }
        msg.push_str(&format!(
            "{} outdated {}",
            output::bold(&casks.to_string()),
            if casks == 1 { "cask" } else { "casks" }
        ));
    }
    println!();
    println!("You have {msg} installed.");
    if report.auto_update {
        return;
    }
    let pronoun = if formulae + casks == 1 { "it" } else { "them" };
    println!(
        "You can upgrade {pronoun} with {}\nor list {pronoun} with {}.",
        output::bold("brew upgrade"),
        output::bold("brew outdated")
    );
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
