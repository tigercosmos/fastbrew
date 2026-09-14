//! `uninstall`/`remove`/`rm` and `autoremove` for formulae.
//!
//! Ports `Homebrew::Uninstall.uninstall_kegs` (`uninstall.rb`), the dependents
//! check of `InstalledDependents.find_some_installed_dependents` with
//! `DependentsMessage#output` (`docs/COMPAT.md` 9), `Keg#uninstall`
//! (`keg.rb`) and `Cleanup.autoremove` (`cleanup.rb`).

use std::collections::BTreeMap;

use crate::api::index::Index;
use crate::config::Config;
use crate::deps;
use crate::error::{Error, Result};
use crate::keg::link::{LinkOptions, remove_records, unlink};
use crate::keg::{self, Keg};
use crate::ops::install::pluralize;
use crate::output;
use crate::resolve::to_sentence;

#[derive(Debug, Clone, Default)]
pub struct UninstallOptions {
    pub force: bool,
    pub ignore_dependencies: bool,
    pub dry_run: bool,
}

pub fn uninstall_formulae(
    cfg: &Config,
    index: &Index,
    names: &[String],
    opts: &UninstallOptions,
) -> Result<()> {
    // Accept names of formulae that are no longer in the API: resolution falls
    // back to the rack on disk.
    let mut racks: Vec<(String, Vec<Keg>)> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for reference in names {
        let name = canonical_installed_name(cfg, index, reference);
        let kegs = keg::installed_kegs(cfg, &name);
        if kegs.is_empty() {
            missing.push(reference.clone());
            continue;
        }
        let selected = if opts.force {
            kegs
        } else {
            // `default_kegs`: the linked keg, else the only installed one.
            // `linked_keg` canonicalises, which would leave the keg path
            // outside `$CELLAR` when the prefix itself sits behind a symlink;
            // rebuild it under the configured Cellar so `unlink` recognises
            // the prefix links as its own.
            match keg::linked_keg(cfg, &name).map(|k| Keg::new(cfg, &name, &k.version.to_string()))
            {
                Some(linked) => vec![linked],
                None if kegs.len() == 1 => kegs,
                None => {
                    let versions: Vec<String> =
                        kegs.iter().map(|k| k.version.to_string()).collect();
                    return Err(Error::user(format!(
                        "{name} has multiple installed versions: {}\nRun `brew uninstall --force {name}` to remove them all.",
                        versions.join(", ")
                    )));
                }
            }
        };
        racks.push((name, selected));
    }

    if !opts.ignore_dependencies && !cfg.no_installed_dependents_check {
        let kegs: Vec<Keg> = racks.iter().flat_map(|(_, k)| k.clone()).collect();
        if let Some((required, dependents)) = find_installed_dependents(cfg, index, &kegs) {
            return Err(Error::user(dependents_message(
                &required,
                &dependents,
                names,
            )));
        }
    }

    for (name, kegs) in &racks {
        let rack = cfg.rack(name);
        if opts.force {
            let (files, bytes) = keg::disk_usage(&rack);
            if opts.dry_run {
                println!(
                    "Would uninstall {}... ({})",
                    rack.display(),
                    crate::cli::fmt::abv(files, bytes)
                );
                continue;
            }
            println!(
                "Uninstalling {}... ({})",
                rack.file_name().unwrap_or_default().to_string_lossy(),
                crate::cli::fmt::abv(files, bytes)
            );
            for k in kegs {
                remove_keg(cfg, index, k)?;
            }
            remove_pin(cfg, name);
            let _ = std::fs::remove_dir(&rack);
            continue;
        }

        for k in kegs {
            if keg::is_pinned(cfg, name) {
                output::onoe(&format!(
                    "{name} is pinned. You must unpin it to uninstall."
                ));
                break;
            }
            let (files, bytes) = keg::disk_usage(&k.path);
            if opts.dry_run {
                println!(
                    "Would uninstall {}... ({})",
                    k.path.display(),
                    crate::cli::fmt::abv(files, bytes)
                );
                continue;
            }
            let _lock = keg::lock::lock_formula(cfg, name)?;
            println!(
                "Uninstalling {}... ({})",
                k.path.display(),
                crate::cli::fmt::abv(files, bytes)
            );
            remove_keg(cfg, index, k)?;
            remove_pin(cfg, name);
            let remaining: Vec<String> = keg::installed_kegs(cfg, name)
                .iter()
                .map(|r| r.version.to_string())
                .collect();
            if remaining.is_empty() {
                let _ = std::fs::remove_dir(&rack);
            } else {
                let verb = if remaining.len() == 1 { "is" } else { "are" };
                println!(
                    "{name} {} {verb} still installed.\nTo remove all versions, run:\n  brew uninstall --force {name}",
                    to_sentence(&remaining, "and")
                );
            }
            warn_about_leftover_config(cfg, index, name);
        }
    }

    if !missing.is_empty() && !opts.force {
        return Err(Error::user(format!(
            "No such keg: {}",
            missing
                .iter()
                .map(|n| cfg.rack(n).display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

/// `Keg#uninstall`: remove the keg directory and every record pointing at it.
fn remove_keg(cfg: &Config, index: &Index, keg: &Keg) -> Result<()> {
    let entry = index.formula(&keg.name);
    let aliases = entry
        .as_ref()
        .map(|e| e.aliases.clone())
        .or_else(|| keg.receipt().ok().map(|r| r.aliases))
        .unwrap_or_default();
    let oldnames = entry.map(|e| e.oldnames).unwrap_or_default();

    let _ = unlink(cfg, keg, LinkOptions::default());
    std::fs::remove_dir_all(&keg.path).map_err(|e| {
        Error::user(format!(
            "Could not remove {} keg! Do so manually:\n  sudo rm -rf {}\n({e})",
            keg.name,
            keg.path.display()
        ))
    })?;
    remove_records(cfg, keg, &aliases, &oldnames)?;
    Ok(())
}

fn remove_pin(cfg: &Config, name: &str) {
    let record = cfg.pinned_record(name);
    if record.is_symlink() {
        let _ = std::fs::remove_file(&record);
    }
    let _ = std::fs::remove_dir(cfg.pinned_kegs());
}

/// Homebrew leaves `$PREFIX/etc/<name>` behind and says so.
fn warn_about_leftover_config(cfg: &Config, index: &Index, name: &str) {
    let pkgetc = cfg.prefix.join("etc").join(deps::short_name(name));
    if !pkgetc.exists() {
        return;
    }
    let mut paths: Vec<String> = walkdir::WalkDir::new(&pkgetc)
        .into_iter()
        .flatten()
        .map(|e| e.path().display().to_string())
        .collect();
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return;
    }
    let _ = index;
    println!();
    output::opoo(&format!(
        "The following {name} configuration files have not been removed!\nIf desired, remove them manually with `rm -rf`:\n  {}",
        paths.join("\n  ")
    ));
}

/// The canonical rack name for a user-supplied reference, accepting formulae
/// that are no longer in the API.
fn canonical_installed_name(cfg: &Config, index: &Index, reference: &str) -> String {
    let bare = deps::short_name(reference).to_string();
    if cfg.rack(&bare).is_dir() {
        return bare;
    }
    if let Some(target) = index.formula_alias(&bare)
        && cfg.rack(&target).is_dir()
    {
        return target;
    }
    if let Some(target) = index.formula_rename(&bare)
        && cfg.rack(&target).is_dir()
    {
        return target;
    }
    bare
}

// ------------------------------------------------------------- dependents

/// `InstalledDependents.find_some_installed_dependents`: the kegs from `kegs`
/// that something still installed needs, and the names of those dependents.
pub fn find_installed_dependents(
    cfg: &Config,
    index: &Index,
    kegs: &[Keg],
) -> Option<(Vec<Keg>, Vec<String>)> {
    let going: Vec<&Keg> = kegs.iter().collect();
    let going_names: Vec<&str> = going.iter().map(|k| k.name.as_str()).collect();

    let mut required: BTreeMap<String, Keg> = BTreeMap::new();
    let mut dependents: Vec<String> = Vec::new();

    for name in keg::installed_formula_names(cfg) {
        if going_names.contains(&name.as_str()) {
            continue;
        }
        let Some(latest) = keg::latest_keg(cfg, &name) else {
            continue;
        };
        let recorded: Vec<String> = latest
            .receipt()
            .map(|r| {
                r.runtime_dependency_names()
                    .into_iter()
                    .map(|f| deps::short_name(f).to_string())
                    .collect()
            })
            .unwrap_or_default();
        let needs: Vec<String> = if recorded.is_empty() {
            deps::recursive_dependency_names(index, &name, deps::DepOptions::default())
        } else {
            recorded
        };
        let mut needed_any = false;
        for keg in &going {
            if needs.contains(&keg.name) {
                required.insert(keg.name.clone(), (*keg).clone());
                needed_any = true;
            }
        }
        if needed_any {
            dependents.push(name);
        }
    }

    // Installed casks count as dependents too.
    for token in deps::installed_cask_tokens(cfg) {
        let Some(cask) = index.cask(&token) else {
            continue;
        };
        let needs: Vec<String> = cask
            .formula_dependencies()
            .iter()
            .map(|d| deps::short_name(d).to_string())
            .collect();
        let mut needed_any = false;
        for keg in &going {
            if needs.contains(&keg.name) {
                required.insert(keg.name.clone(), (*keg).clone());
                needed_any = true;
            }
        }
        if needed_any {
            dependents.push(token);
        }
    }

    if required.is_empty() || dependents.is_empty() {
        return None;
    }
    dependents.sort();
    dependents.dedup();
    Some((required.into_values().collect(), dependents))
}

/// `DependentsMessage#output` (`docs/COMPAT.md` 9).
pub fn dependents_message(required: &[Keg], dependents: &[String], named: &[String]) -> String {
    let paths: Vec<String> = required
        .iter()
        .map(|k| k.path.display().to_string())
        .collect();
    let one_req = required.len() == 1;
    let one_dep = dependents.len() == 1;
    format!(
        "Refusing to uninstall {}\nbecause {} {} required by {}, which {} currently installed.\nYou can override this and force removal with:\n  brew uninstall --ignore-dependencies {}",
        to_sentence(&paths, "and"),
        if one_req { "it" } else { "they" },
        if one_req { "is" } else { "are" },
        to_sentence(dependents, "and"),
        if one_dep { "is" } else { "are" },
        named.join(" ")
    )
}

// ------------------------------------------------------------- autoremove

/// `Utils.pluralize("formula", n)` without the count, which the autoremove
/// heading prints itself.
fn formulae(count: usize) -> String {
    pluralize("formula", count)
        .split_once(' ')
        .map(|(_, stem)| stem.to_string())
        .unwrap_or_default()
}

/// `Cleanup.autoremove`.
pub fn autoremove(cfg: &Config, index: &Index, dry_run: bool) -> Result<()> {
    let mut removable: Vec<String> = deps::removable(cfg, index)?.into_iter().collect();
    // Homebrew drops candidates that something still installed needs.
    let candidates: Vec<Keg> = removable
        .iter()
        .filter_map(|n| keg::latest_keg(cfg, n))
        .collect();
    if let Some((required, _)) = find_installed_dependents(cfg, index, &candidates) {
        let keep: Vec<String> = required.iter().map(|k| k.name.clone()).collect();
        removable.retain(|n| !keep.contains(n));
    }
    if removable.is_empty() {
        return Ok(());
    }
    removable.sort();

    let verb = if dry_run {
        "Would autoremove"
    } else {
        "Autoremoving"
    };
    output::ohai(&format!(
        "{verb} {} unneeded {}:",
        removable.len(),
        formulae(removable.len())
    ));
    println!("{}", removable.join("\n"));
    if dry_run {
        return Ok(());
    }
    uninstall_formulae(
        cfg,
        index,
        &removable,
        &UninstallOptions {
            force: false,
            ignore_dependencies: true,
            dry_run: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keg(path: &str) -> Keg {
        Keg {
            name: path.rsplit('/').nth(1).unwrap_or("x").to_string(),
            version: crate::version::PkgVersion::parse(path.rsplit('/').next().unwrap_or("1")),
            path: std::path::PathBuf::from(path),
        }
    }

    #[test]
    fn dependents_block_matches_compat_9() {
        let message = dependents_message(
            &[keg("/opt/homebrew/Cellar/oniguruma/6.9.10")],
            &["jq".to_string()],
            &["oniguruma".to_string()],
        );
        assert_eq!(
            message,
            "Refusing to uninstall /opt/homebrew/Cellar/oniguruma/6.9.10\n\
             because it is required by jq, which is currently installed.\n\
             You can override this and force removal with:\n  \
             brew uninstall --ignore-dependencies oniguruma"
        );
    }

    #[test]
    fn several_dependents_are_joined_with_and() {
        let message = dependents_message(
            &[keg("/p/Cellar/a/1"), keg("/p/Cellar/b/2")],
            &["x".to_string(), "y".to_string(), "z".to_string()],
            &["a".to_string(), "b".to_string()],
        );
        assert!(
            message.starts_with("Refusing to uninstall /p/Cellar/a/1 and /p/Cellar/b/2\nbecause they are required by x, y and z, which are currently installed."),
            "{message}"
        );
        assert!(message.ends_with("brew uninstall --ignore-dependencies a b"));
    }
}
