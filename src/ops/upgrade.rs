//! `upgrade` for formulae: install the new version, unlink the old, relink,
//! migrate pinned/linked state, then clean up old kegs unless
//! `HOMEBREW_NO_INSTALL_CLEANUP`.
//!
//! Ports `Homebrew::Upgrade` (`upgrade.rb`) and the outdated/pinned partition
//! of `cmd/upgrade.rb#upgrade_outdated_formulae`.

use crate::api::index::Index;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::keg;
use crate::ops::install::{self, InstallOptions, Mode, pluralize};
use crate::ops::outdated::{self, OutdatedFormula};
use crate::output;

#[derive(Debug, Clone, Default)]
pub struct UpgradeOptions {
    pub dry_run: bool,
    pub force: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub greedy: bool,
}

/// Empty `names` means everything outdated.
pub fn upgrade_formulae(
    cfg: &Config,
    index: &Index,
    names: &[String],
    opts: &UpgradeOptions,
) -> Result<()> {
    let named = !names.is_empty();
    let outdated = outdated::outdated_formulae(cfg, index, Some(names))?;

    if named {
        report_not_outdated(cfg, index, names, opts)?;
    }
    if outdated.is_empty() {
        return Ok(());
    }

    let (pinned, upgradeable): (Vec<_>, Vec<_>) = outdated.into_iter().partition(|f| f.pinned);

    if upgradeable.is_empty() {
        if !opts.quiet {
            output::ohai("No packages to upgrade");
        }
    } else {
        let verb = if opts.dry_run {
            "Would upgrade"
        } else {
            "Upgrading"
        };
        output::ohai(&format!(
            "{verb} {} outdated {}:",
            upgradeable.len(),
            if upgradeable.len() == 1 {
                "package"
            } else {
                "packages"
            }
        ));
        println!("{}", format_upgrade_summary(&descriptions(&upgradeable)));
    }

    if !pinned.is_empty() {
        let message = format!(
            "Not upgrading {} pinned {}:",
            pinned.len(),
            if pinned.len() == 1 {
                "package"
            } else {
                "packages"
            }
        );
        // Naming a pinned formula explicitly is an error; a bare `upgrade`
        // only warns.
        if named {
            output::onoe(&message);
        } else {
            output::opoo(&message);
        }
        println!(
            "{}",
            pinned
                .iter()
                .map(|f| format!("{} {}", f.name, f.current_version))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    if upgradeable.is_empty() {
        return Ok(());
    }
    if opts.dry_run {
        return Ok(());
    }

    let mut failed: Vec<String> = Vec::new();
    for formula in &upgradeable {
        if !opts.quiet {
            output::ohai(&format!("Upgrading {}", formula.name));
            // `Upgrade.print_upgrade_message` joins the (empty) option list
            // after the version transition, so the line ends in a space.
            println!("  {} ", version_transition(cfg, formula));
        }
        let install_opts = InstallOptions {
            force: opts.force,
            quiet: opts.quiet,
            verbose: opts.verbose,
            // `installed_on_request` is carried over from the old receipt.
            on_request: crate::deps::installed_on_request(cfg, &formula.name),
            ..Default::default()
        };
        let result = install::install_formulae_with(
            cfg,
            index,
            std::slice::from_ref(&formula.name),
            &install_opts,
            Mode { upgrade: true },
        );
        match result {
            Ok(()) => migrate_pin(cfg, &formula.name),
            Err(e) => {
                output::onoe(&format!("{}: {e}", formula.name));
                failed.push(formula.name.clone());
            }
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(Error::user(format!(
            "Failed to upgrade {}",
            crate::resolve::to_sentence(&failed, "and")
        )))
    }
}

/// `cmd/upgrade.rb`: named formulae that are not outdated get a notice.
fn report_not_outdated(
    cfg: &Config,
    index: &Index,
    names: &[String],
    opts: &UpgradeOptions,
) -> Result<()> {
    for name in names {
        if !outdated::outdated_kegs(cfg, index, name).is_empty() {
            continue;
        }
        match keg::latest_keg(cfg, name) {
            None => output::onoe(&format!("{name} not installed")),
            Some(k) if !opts.quiet => {
                output::opoo(&format!("{name} {} already installed", k.version));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// `cmd/upgrade.rb#formula_upgrade_descriptions`.
fn descriptions(outdated: &[OutdatedFormula]) -> Vec<String> {
    outdated
        .iter()
        .map(|f| {
            format!(
                "{} {} -> {}",
                f.name,
                f.installed_versions.last().cloned().unwrap_or_default(),
                f.current_version
            )
        })
        .collect()
}

fn version_transition(cfg: &Config, formula: &OutdatedFormula) -> String {
    match keg::linked_keg(cfg, &formula.name) {
        Some(k) => format!("{} -> {}", k.version, formula.current_version),
        None => format!("-> {}", formula.current_version),
    }
}

/// `Upgrade.format_upgrade_summary`: a single entry is printed as is, several
/// are column-aligned on the name and the old version.
pub fn format_upgrade_summary(upgrades: &[String]) -> String {
    if upgrades.len() < 2 {
        return upgrades.join("\n");
    }
    let name_width = upgrades
        .iter()
        .map(|u| u.split_once(' ').map(|(n, _)| n.len()).unwrap_or(u.len()))
        .max()
        .unwrap_or(0);
    let old_width = upgrades
        .iter()
        .filter_map(|u| u.split_once(' ').map(|(_, v)| v))
        .filter_map(|versions| versions.split_once(" -> ").map(|(old, _)| old.len()))
        .max()
        .unwrap_or(0);
    upgrades
        .iter()
        .map(|u| {
            let (name, versions) = match u.split_once(' ') {
                Some((n, v)) => (n, v),
                None => return u.clone(),
            };
            if versions.is_empty() {
                return name.to_string();
            }
            match versions.split_once(" -> ") {
                Some((old, new)) => format!(
                    "{:name_width$}  {:old_width$} -> {new}",
                    name,
                    old,
                    name_width = name_width,
                    old_width = old_width
                ),
                None => format!("{name:name_width$}  {versions}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `FormulaPin#pin_at`: keep a pin pointing at a keg that still exists.
fn migrate_pin(cfg: &Config, name: &str) {
    let record = cfg.pinned_record(name);
    if !record.is_symlink() {
        return;
    }
    if std::fs::canonicalize(&record).is_ok_and(|p| p.is_dir()) {
        return;
    }
    let Some(latest) = keg::latest_keg(cfg, name) else {
        let _ = std::fs::remove_file(&record);
        return;
    };
    let _ = std::fs::remove_file(&record);
    let _ = keg::make_relative_symlink(&record, &latest.path);
}

/// `Utils.pluralize("package", n)` for the headers above.
#[allow(dead_code)]
fn packages(count: usize) -> String {
    pluralize("package", count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_upgrade_is_printed_verbatim() {
        assert_eq!(
            format_upgrade_summary(&["jq 1.8.1 -> 1.8.2".to_string()]),
            "jq 1.8.1 -> 1.8.2"
        );
    }

    #[test]
    fn several_upgrades_are_column_aligned() {
        let summary = format_upgrade_summary(&[
            "jq 1.8.1 -> 1.8.2".to_string(),
            "oniguruma 6.9.9 -> 6.9.10".to_string(),
        ]);
        assert_eq!(
            summary,
            "jq         1.8.1 -> 1.8.2\noniguruma  6.9.9 -> 6.9.10"
        );
    }
}
