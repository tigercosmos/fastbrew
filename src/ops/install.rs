//! `install` and `reinstall` for formulae (bottles only; source builds delegate).
//!
//! Order of operations follows `docs/DESIGN.md` 6 and
//! `Library/Homebrew/formula_installer.rb` (`install`, `pour`, `finish`,
//! `link`, `install_service`, `caveats`, `summary`), with the already-installed
//! messages of `Homebrew::Install.install_formula?` (`install/check.rb`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use rayon::prelude::*;

use crate::api::index::Index;
use crate::bottle::fetch::{self, ManifestInfo};
use crate::bottle::{BottleRef, BottleTab, extract, relocate};
use crate::config::Config;
use crate::deps::{self, DepOptions};
use crate::error::{Error, Result};
use crate::keg;
use crate::keg::link::LinkOptions;
use crate::keg::lock::{self, Lock};
use crate::model::FormulaEntry;
use crate::ops::plan::{self, Action, Already, Item};
use crate::ops::{caveats, cleanup, postinstall, receipt};
use crate::output;
use crate::resolve;

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub only_dependencies: bool,
    pub ignore_dependencies: bool,
    pub force: bool,
    pub dry_run: bool,
    pub overwrite: bool,
    pub skip_post_install: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub reinstall: bool,
    /// Mark as installed on request (false for dependencies).
    pub on_request: bool,
    pub build_from_source: bool,
    pub head: bool,
    pub keep_tmp: bool,
}

/// Extra knobs the CLI does not set, so that `InstallOptions` keeps exactly the
/// fields `src/cli/mutate.rs` builds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mode {
    /// `upgrade` prints its own `==> Upgrading <name>` header and version line.
    pub upgrade: bool,
}

/// `Utils.pluralize(stem, count, include_count: true)`, for the stems the
/// install path uses: `formula` takes `-e`, `dependency` becomes `dependencies`.
pub fn pluralize(stem: &str, count: usize) -> String {
    let (stem, singular, plural) = match stem {
        "formula" => ("formula", "", "e"),
        "dependency" => ("dependenc", "y", "ies"),
        other => (other, "", "s"),
    };
    let suffix = if count == 1 { singular } else { plural };
    format!("{count} {stem}{suffix}")
}

/// Resolve `names` and install them.
///
/// Kept for callers that only have a reference to work from (the cask
/// installer's `depends_on formula:`); everything that has already resolved a
/// formula passes the entry to [`install_formulae_entries`] instead, so an
/// explicitly tapped name is never re-resolved to a core formula.
pub fn install_formulae(
    cfg: &Config,
    index: &Index,
    names: &[String],
    opts: &InstallOptions,
) -> Result<()> {
    let mut roots: Vec<FormulaEntry> = Vec::new();
    for name in names {
        roots.push(resolve::resolve_formula(cfg, index, name)?);
    }
    install_formulae_entries(cfg, index, &roots, opts)
}

/// Plan and execute installation of already-resolved formulae.
pub fn install_formulae_entries(
    cfg: &Config,
    index: &Index,
    roots: &[FormulaEntry],
    opts: &InstallOptions,
) -> Result<()> {
    install_formulae_with(cfg, index, roots, opts, Mode::default())
}

/// `install_formulae_entries` with the extra [`Mode`] knobs `upgrade` needs.
pub fn install_formulae_with(
    cfg: &Config,
    index: &Index,
    roots: &[FormulaEntry],
    opts: &InstallOptions,
    mode: Mode,
) -> Result<()> {
    for formula in roots {
        if let Some(message) = plan::check_deprecate_disable(formula, opts.force)? {
            output::opoo(&message);
        }
    }

    // Planning only reads: no receipt is written and no lock is taken, so
    // `--dry-run` leaves the prefix exactly as it found it.
    let plan = build_plan(cfg, index, roots, opts, mode)?;

    if opts.dry_run {
        print_dry_run(cfg, &plan, opts, mode);
        return Ok(());
    }

    // 3. Take formula locks for everything this run touches, including the
    // racks whose receipt only needs its `installed_on_request` flag flipped.
    let locks = take_locks(cfg, &plan)?;
    for name in &plan.mark_on_request {
        mark_installed_on_request(cfg, name);
    }
    if plan.items.is_empty() {
        // Everything was already installed; the warnings are already printed.
        drop(locks);
        return finish_run(cfg, index, &plan, opts);
    }
    let _locks = locks;

    // 4. Fetch every manifest and blob concurrently.
    let downloads = fetch_plan(cfg, &plan, opts)?;

    // Refuse fixed-cellar bottles this prefix is too long for, before touching
    // the Cellar (`pour_bottle?`'s `compatible_locations?` check).
    for item in &plan.items {
        let Some(download) = downloads.get(item.name()) else {
            continue;
        };
        plan::check_relocatable(cfg, &item.formula, &download.manifest.tab)?;
    }

    // 5-7. Extract and relocate, in parallel: each keg is independent.
    let mut poured = pour_all(cfg, &plan, &downloads, opts);

    // 8-14. Finish each keg in dependency order.
    let mut failed: HashSet<String> = HashSet::new();
    let mut headed: HashSet<String> = HashSet::new();
    let mut messages: Vec<(String, caveats::Caveats)> = Vec::new();
    let mut link_failures: Vec<String> = Vec::new();

    for item in &plan.items {
        let poured_keg = match poured.remove(item.name()) {
            Some(Ok(keg)) => keg,
            Some(Err(error)) => {
                output::onoe(&format!("{}: {error}", item.name()));
                failed.insert(item.name().to_string());
                continue;
            }
            None => {
                output::onoe(&format!("{}: nothing was poured", item.name()));
                failed.insert(item.name().to_string());
                continue;
            }
        };
        if depends_on_failed(item, &failed) {
            output::onoe(&format!(
                "{}: skipped because a dependency failed to install",
                item.name()
            ));
            // Its bottle is already unpacked: steps 4 and 5 run for the whole
            // plan before any of it is finished. Leaving the keg would make
            // the next run believe the formula is installed, so it goes, and
            // whatever it replaced comes back.
            discard_unfinished_keg(cfg, item);
            restore_backup(cfg, item, poured_keg);
            failed.insert(item.name().to_string());
            continue;
        }

        if !item.requested && !opts.quiet && headed.insert(item.root.clone()) {
            let dep_names = plan.dependencies_of(&item.root);
            output::ohai(&format!(
                "Installing dependencies for {}: {}",
                item.root,
                resolve::to_sentence(&dep_names, "and")
            ));
        }
        print_install_header(item, opts, mode);
        if !opts.quiet {
            // `ohai "Pouring #{downloadable.downloader.basename}"`: the
            // bottle's own name, not the hashed cache file name.
            output::ohai(&format!("Pouring {}", item.bottle.filename()));
        }

        match finish_item(cfg, index, &plan, item, &downloads, opts) {
            Ok(outcome) => {
                // `Reinstall.reinstall_formula`'s success branch: the keg this
                // run replaced only goes once finishing returned. A link that
                // did not work out is not a failure here, exactly as it is not
                // one for Homebrew.
                if let Some(backup) = &poured_keg.backup {
                    backup.discard();
                }
                if let Some(block) = outcome.link_failed {
                    link_failures.push(block);
                }
                if !outcome.caveats.is_empty() {
                    messages.push((item.name().to_string(), outcome.caveats));
                }
                let (files, bytes) = keg::disk_usage(&item.keg(cfg).path);
                println!("{}", summary_line(cfg, &item.keg(cfg).path, files, bytes));
            }
            Err(e) => {
                output::onoe(&format!("{}: {e}", item.name()));
                discard_unfinished_keg(cfg, item);
                restore_backup(cfg, item, poured_keg);
                failed.insert(item.name().to_string());
            }
        }
    }

    // `Homebrew::Install.finish_installation` cleans up first and displays the
    // recorded caveats last. Cleanup takes the same formula locks to remove an
    // old keg, so this run has to let go of them first.
    drop(_locks);
    finish_run(cfg, index, &plan, opts)?;
    print_messages(&messages);

    // Homebrew `ofail`s these: it prints the block and exits 1. Returning them
    // as one error prints each block exactly once and keeps the exit status.
    let mut blocks = link_failures;
    if !failed.is_empty() {
        let mut names: Vec<String> = failed.into_iter().collect();
        names.sort();
        blocks.push(format!(
            "Failed to install {}",
            resolve::to_sentence(&names, "and")
        ));
    }
    if blocks.is_empty() {
        Ok(())
    } else {
        Err(Error::user(blocks.join("\n")))
    }
}

/// `Homebrew::Install.install_formulae`'s `--dry-run` branch: the requested
/// formulae on one line, then one block per formula for its dependencies,
/// split into the ones that would be installed and the ones upgraded.
fn print_dry_run(cfg: &Config, plan: &Plan, opts: &InstallOptions, mode: Mode) {
    let verb = if opts.reinstall {
        "reinstall"
    } else if mode.upgrade {
        "upgrade"
    } else {
        "install"
    };
    // In the order the user named them, and once each: a formula that is both
    // requested and another root's dependency is one item, not two.
    let requested: Vec<String> = plan
        .roots
        .iter()
        .filter_map(|root| {
            plan.items
                .iter()
                .find(|i| i.requested && i.formula.full_name() == *root)
                .map(|i| i.name().to_string())
        })
        .collect();
    if !requested.is_empty() {
        output::ohai(&format!(
            "Would {verb} {}:",
            pluralize("formula", requested.len())
        ));
        println!("{}", requested.join(" "));
    }

    for root in &plan.roots {
        let deps: Vec<&Item> = plan
            .items
            .iter()
            .filter(|i| !i.requested && i.root == *root)
            .collect();
        if deps.is_empty() {
            continue;
        }
        for (verb, wanted) in [("install", Action::Install), ("upgrade", Action::Upgrade)] {
            let group: Vec<String> = deps
                .iter()
                .filter(|i| i.action == wanted)
                .map(|i| dry_run_description(cfg, i))
                .collect();
            if group.is_empty() {
                continue;
            }
            output::ohai(&format!(
                "Would {verb} {} for {root}:",
                pluralize("dependency", group.len())
            ));
            println!("{}", crate::ops::upgrade::format_upgrade_summary(&group));
        }
    }
}

/// `Upgrade.upgrade_formula`'s dry-run label: `name old -> new` for something
/// already installed, `name version` otherwise.
fn dry_run_description(cfg: &Config, item: &Item) -> String {
    let name = item.formula.full_name();
    let new = item.pkg_version();
    match keg::latest_keg(cfg, item.name()).map(|k| k.version.to_string()) {
        Some(current) if current != new => format!("{name} {current} -> {new}"),
        _ => format!("{name} {new}"),
    }
}

/// `fetch`: download manifests and blobs only.
pub fn fetch_formulae(
    cfg: &Config,
    index: &Index,
    roots: &[FormulaEntry],
    with_deps: bool,
    force: bool,
) -> Result<()> {
    let mut entries: Vec<FormulaEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for formula in roots {
        if with_deps {
            for dep in deps::recursive_dependencies(cfg, index, formula, DepOptions::default())? {
                if seen.insert(dep.full_name()) {
                    entries.push(dep);
                }
            }
        }
        if seen.insert(formula.full_name()) {
            entries.push(formula.clone());
        }
    }

    let mut bottles: Vec<BottleRef> = Vec::new();
    for entry in &entries {
        let bottle = plan::require_bottle(cfg, entry)?;
        if force && let Some(path) = fetch::cached_blob_path(cfg, &bottle) {
            let _ = std::fs::remove_file(path);
        }
        output::ohai(&format!("Fetching {}", entry.full_name()));
        bottles.push(bottle);
    }

    let mut failures = Vec::new();
    for (entry, result) in entries.iter().zip(fetch::fetch_all(cfg, &bottles, false)) {
        if let Err(e) = result {
            output::onoe(&format!("{}: {e}", entry.name));
            failures.push(entry.name.clone());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::user(format!(
            "Failed to fetch {}",
            resolve::to_sentence(&failures, "and")
        )))
    }
}

// --------------------------------------------------------------- planning

/// Everything one `install` run will do.
///
/// `items` is deduplicated and in dependency order: every formula appears at
/// most once, after everything it depends on. A formula the user named stays a
/// requested item even when another named formula depends on it.
#[derive(Debug, Default)]
pub struct Plan {
    pub items: Vec<Item>,
    /// Full names of the formulae the user asked for, in order.
    pub roots: Vec<String>,
    /// Racks that are already installed at the wanted version but whose
    /// receipt has to record the explicit request. Applied under the lock.
    pub mark_on_request: Vec<String>,
}

impl Plan {
    /// Names of the dependencies this run installs for `root`, in install order.
    fn dependencies_of(&self, root: &str) -> Vec<String> {
        self.items
            .iter()
            .filter(|i| !i.requested && i.root == root)
            .map(|i| i.name().to_string())
            .collect()
    }

    /// pkg_version this run installs for `name`, if any.
    fn planned_version(&self, name: &str) -> Option<String> {
        self.items
            .iter()
            .find(|i| i.name() == name)
            .map(Item::pkg_version)
    }
}

fn build_plan(
    cfg: &Config,
    index: &Index,
    roots: &[FormulaEntry],
    opts: &InstallOptions,
    mode: Mode,
) -> Result<Plan> {
    let mut plan = Plan {
        roots: roots.iter().map(FormulaEntry::full_name).collect(),
        ..Default::default()
    };
    let mut wanted: Vec<(FormulaEntry, Action)> = Vec::new();

    for root in roots {
        let action = if opts.reinstall {
            Action::Reinstall
        } else if mode.upgrade {
            Action::Upgrade
        } else {
            match plan::already_installed(cfg, root, opts.only_dependencies) {
                Already::NotInstalled => Action::Install,
                Already::Outdated => Action::Upgrade,
                Already::Installed { notice } => {
                    if !opts.quiet {
                        notice.print();
                    }
                    // Homebrew still records the explicit request, but only
                    // once the run holds this rack's lock.
                    plan.mark_on_request.push(root.name.clone());
                    continue;
                }
            }
        };
        plan::check_conflicts(cfg, root)?;
        wanted.push((root.clone(), action));
    }

    // One ordered, deduplicated list: each requested formula is preceded by
    // the dependencies this run has to install for it, and a formula that is
    // both requested and another root's dependency appears exactly once, as
    // the requested item.
    for (root, action) in &wanted {
        let root_name = root.full_name();
        for entry in plan::dependency_closure(cfg, index, root, opts.ignore_dependencies)? {
            let Some(dep_action) = plan::dependency_action(cfg, &entry) else {
                continue;
            };
            // `installed_on_request` stays true when an existing receipt says so.
            let on_request = deps::installed_on_request(cfg, &entry.name);
            let item = make_item(cfg, index, &entry, dep_action, Some(&root_name), on_request)?;
            add_item(&mut plan.items, item)?;
        }
        if opts.only_dependencies {
            continue;
        }
        let item = make_item(
            cfg,
            index,
            root,
            *action,
            None,
            opts.on_request || opts.reinstall,
        )?;
        add_item(&mut plan.items, item)?;
    }
    Ok(plan)
}

/// Add `item` to the plan, or merge it into the entry already there.
///
/// Identity is the full name (`tap/name`), not the bare name: two taps can
/// carry a `jq`, and they are different packages that happen to want the same
/// rack. The first occurrence fixes the position, which keeps the list in
/// dependency order. A formula that first appeared as a dependency and is then
/// named on the command line is promoted to a requested item, so the run does
/// the work once and the receipt records the request.
///
/// One rack cannot hold two packages, so a collision fails the run while
/// planning, before anything is locked, fetched or written.
fn add_item(items: &mut Vec<Item>, item: Item) -> Result<()> {
    let full_name = item.formula.full_name();
    if let Some(existing) = items
        .iter_mut()
        .find(|i| i.formula.full_name() == full_name)
    {
        if item.requested && !existing.requested {
            existing.requested = true;
            existing.root = String::new();
            existing.installed_on_request = true;
            existing.action = item.action;
        }
        return Ok(());
    }
    if let Some(other) = items.iter().find(|i| i.name() == item.name()) {
        return Err(Error::user(format!(
            "Formulae with the same name from different taps cannot be installed \
             at the same time:\n       * {}\n       * {full_name}\n\nInstall them one \
             at a time, uninstalling the other first:\n  brew uninstall {}",
            other.formula.full_name(),
            item.name()
        )));
    }
    items.push(item);
    Ok(())
}

fn make_item(
    cfg: &Config,
    index: &Index,
    formula: &FormulaEntry,
    action: Action,
    root: Option<&str>,
    installed_on_request: bool,
) -> Result<Item> {
    let bottle = plan::require_bottle(cfg, formula)?;
    let existing = keg::installed_kegs(cfg, &formula.name);
    let was_linked = existing.iter().any(|k| k.is_linked(cfg));
    let dependency_names =
        deps::entry_dependency_names(index, formula, DepOptions::default(), true)
            .iter()
            .map(|d| deps::short_name(d).to_string())
            .collect();
    Ok(Item {
        cellar: formula.bottle_cellar_kind(),
        formula: formula.clone(),
        bottle,
        action,
        requested: root.is_none(),
        root: root.unwrap_or_default().to_string(),
        installed_on_request,
        existing,
        was_linked,
        dependency_names,
    })
}

/// `install/check.rb`: a formula the user asked for but that is already there
/// still stops being "installed as a dependency".
///
/// This writes a receipt, so it only ever runs with the rack's formula lock
/// held — never while planning, and never on a `--dry-run`.
fn mark_installed_on_request(cfg: &Config, name: &str) {
    let Some(keg) = keg::linked_keg(cfg, name).or_else(|| keg::latest_keg(cfg, name)) else {
        return;
    };
    let Ok(mut receipt) = keg.receipt() else {
        return;
    };
    if receipt.installed_on_request {
        return;
    }
    receipt.installed_on_request = true;
    let _ = receipt.write(&keg.receipt_path());
}

/// `FormulaInstaller#lock`: one `<name>.formula.lock` per rack this run will
/// write to, taken before the first byte is poured and held until the kegs are
/// finished. Contention is `OperationInProgressError`.
fn take_locks(cfg: &Config, plan: &Plan) -> Result<Vec<Lock>> {
    let mut names: Vec<&str> = plan
        .items
        .iter()
        .map(Item::name)
        .chain(plan.mark_on_request.iter().map(String::as_str))
        .collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .map(|n| lock::lock_formula(cfg, n))
        .collect()
}

// ---------------------------------------------------------------- fetching

struct Download {
    manifest: ManifestInfo,
    blob: PathBuf,
}

fn fetch_plan(
    cfg: &Config,
    plan: &Plan,
    opts: &InstallOptions,
) -> Result<HashMap<String, Download>> {
    if !opts.quiet {
        for root in &plan.roots {
            let deps = plan.dependencies_of(root);
            if deps.is_empty() {
                continue;
            }
            output::ohai(&format!(
                "Fetching dependencies for {root}: {}",
                resolve::to_sentence(&deps, "and")
            ));
        }
    }
    if !opts.quiet {
        for item in &plan.items {
            output::ohai(&format!("Fetching {}", item.formula.full_name()));
        }
    }

    let bottles: Vec<BottleRef> = plan.items.iter().map(|i| i.bottle.clone()).collect();
    let results = fetch::fetch_all(cfg, &bottles, opts.quiet);
    let mut out = HashMap::new();
    let mut failures = Vec::new();
    for (item, result) in plan.items.iter().zip(results) {
        match result {
            Ok((manifest, blob)) => {
                out.insert(item.name().to_string(), Download { manifest, blob });
            }
            Err(e) => {
                output::onoe(&format!("{}: {e}", item.name()));
                failures.push(item.name().to_string());
            }
        }
    }
    if !failures.is_empty() {
        return Err(Error::user(format!(
            "Failed to download {}",
            resolve::to_sentence(&failures, "and")
        )));
    }
    Ok(out)
}

// ----------------------------------------------------------------- pouring

/// What one pour left for the finishing step to keep or undo.
struct Poured {
    /// The keg this pour displaced, kept at `<version>.reinstall` until the
    /// install finished (`Homebrew::Reinstall.backup`).
    backup: Option<extract::Backup>,
    /// Whether that keg was linked before the pour unlinked it, so a restore
    /// can put the link state back too (`Reinstall.restore_backup`).
    was_linked: bool,
}

/// Extract and relocate every keg; independent formulae run in parallel.
fn pour_all(
    cfg: &Config,
    plan: &Plan,
    downloads: &HashMap<String, Download>,
    opts: &InstallOptions,
) -> HashMap<String, Result<Poured>> {
    // `reinstall` and `upgrade` replace a keg that may still be linked. An
    // upgrade can land on a directory that is already there: `outdated` counts
    // the current version as outdated while it is neither linked nor
    // opt-linked, so `upgrade` pours the very version that sits in the rack.
    // Whether it was linked has to be read before it is unlinked, so a restore
    // can put the prefix back the way it found it.
    let mut was_linked: HashMap<&str, bool> = HashMap::new();
    for item in &plan.items {
        for existing in &item.existing {
            if existing.version.to_string() == item.pkg_version() {
                let linked = existing.is_linked(cfg);
                *was_linked.entry(item.name()).or_default() |= linked;
                let _ = keg::link::unlink(cfg, existing, LinkOptions::default());
            }
        }
    }

    let results: Vec<(String, Result<Poured>)> = plan
        .items
        .par_iter()
        .map(|item| {
            let name = item.name().to_string();
            let Some(download) = downloads.get(&name) else {
                return (name, Err(Error::user("no download for this formula")));
            };
            let linked = was_linked.get(item.name()).copied().unwrap_or(false);
            (name, pour_one(cfg, item, download, linked, opts))
        })
        .collect();
    results.into_iter().collect()
}

/// `Homebrew::Reinstall.restore_backup`: put the displaced keg back where it
/// was and relink it when the pour had unlinked it.
///
/// The opt record always comes back — `unlink` removed it and every installed
/// keg has one — while the prefix links only do when the keg had them.
fn restore_backup(cfg: &Config, item: &Item, poured: Poured) {
    let Some(backup) = poured.backup else {
        return;
    };
    if let Err(e) = backup.restore() {
        output::onoe(&format!(
            "{}: could not put the previous keg back: {e}",
            item.name()
        ));
        return;
    }
    let keg = item.keg(cfg);
    let _ = keg::link::optlink(cfg, &keg, &item.formula.aliases, &item.formula.oldnames);
    if poured.was_linked {
        let _ = keg::link::link(
            cfg,
            &keg,
            &item.formula.link_overwrite_paths,
            LinkOptions::default(),
        );
    }
}

/// Remove a keg this run unpacked but never finished.
///
/// A directory in the rack without `INSTALL_RECEIPT.json` is not an
/// installation: `FormulaInstaller#install` removes the keg when anything after
/// the pour raises, and nothing else ever creates one. Keeping it would make
/// the next `install` report "already installed, it's just not linked" and stop
/// without ever finishing the work.
///
/// [`finish_item`] writes the receipt as its last step, so this is exactly the
/// test for "this keg was never finished", whether the run failed or was
/// killed.
fn discard_unfinished_keg(cfg: &Config, item: &Item) {
    let keg = item.keg(cfg);
    if !keg.path.is_dir() || keg.receipt_path().is_file() {
        return;
    }
    // `Keg#ignore_interrupts_and_uninstall!`: the links go with the keg. A
    // record left pointing at a keg that is no longer there would make the
    // next attempt's `link` refuse with "Another version is already linked".
    // The aliases come from the formula: there is no receipt to read them from.
    let aliases = &item.formula.aliases;
    let _ = keg::link::unlink_with_aliases(cfg, &keg, aliases, LinkOptions::default());
    let _ = std::fs::remove_dir_all(&keg.path);
    let _ = keg::link::remove_records(cfg, &keg, aliases, &item.formula.oldnames);
    // `remove_dir` only succeeds on an empty rack, which is what we want.
    let _ = std::fs::remove_dir(cfg.rack(item.name()));
}

fn pour_one(
    cfg: &Config,
    item: &Item,
    download: &Download,
    was_linked: bool,
    _opts: &InstallOptions,
) -> Result<Poured> {
    let pkg_version = item.pkg_version();
    let replace = item.action == Action::Reinstall
        || item
            .existing
            .iter()
            .any(|k| k.version.to_string() == pkg_version);
    let pour = extract::extract_bottle(cfg, &download.blob, item.name(), &pkg_version, replace)?;

    let tab = receipt::tab_with_keg_fallback(&download.manifest.tab, &pour.keg);
    let openjdk = openjdk_dependency(&tab);
    let result = relocate::relocate_keg(
        cfg,
        relocate::RelocateArgs {
            keg_path: &pour.keg,
            cellar_kind: &item.cellar,
            tab: &tab,
            openjdk_dep: openjdk.as_deref(),
        },
    );
    match result {
        Ok(_) => Ok(Poured {
            backup: pour.backup,
            was_linked,
        }),
        Err(e) => {
            // `FormulaInstaller#install` removes a keg that failed to pour,
            // and `Reinstall` puts the keg it replaced back.
            let _ = std::fs::remove_dir_all(&pour.keg);
            restore_backup(
                cfg,
                item,
                Poured {
                    backup: pour.backup,
                    was_linked,
                },
            );
            Err(e)
        }
    }
}

/// `@@HOMEBREW_JAVA@@` expands through the `openjdk(@n)?` runtime dependency.
fn openjdk_dependency(tab: &BottleTab) -> Option<String> {
    tab.runtime_dependencies
        .iter()
        .map(|d| deps::short_name(&d.full_name).to_string())
        .find(|n| n == "openjdk" || n.starts_with("openjdk@"))
}

struct Outcome {
    caveats: caveats::Caveats,
    /// Homebrew's `brew link` failure block, when linking did not work out.
    link_failed: Option<String>,
}

/// Steps 6 and 9-14 of `docs/DESIGN.md` 6 for one keg.
fn finish_item(
    cfg: &Config,
    index: &Index,
    plan: &Plan,
    item: &Item,
    downloads: &HashMap<String, Download>,
    opts: &InstallOptions,
) -> Result<Outcome> {
    let keg = item.keg(cfg);
    let tab = downloads
        .get(item.name())
        .map(|d| receipt::tab_with_keg_fallback(&d.manifest.tab, &keg.path))
        .unwrap_or_default();

    // 6. The receipt's fields. Its `time` is when the keg was poured, so it is
    // taken here; the file itself is written last (step 13).
    let installed_on_request = match item.action {
        // `reinstall`/`upgrade` keep the old receipt's flag.
        Action::Reinstall | Action::Upgrade => {
            item.installed_on_request || previous_on_request(item)
        }
        Action::Install => item.installed_on_request,
    };
    let poured_at = receipt::now();

    // 9. optlink, then link unless keg-only.
    let aliases = item.formula.aliases.clone();
    keg::link::optlink(cfg, &keg, &aliases, &item.formula.oldnames)?;
    // `Homebrew::Install.install_formula` unlinks the kegs being replaced
    // before the new one is linked, so their symlinks never collide.
    for existing in &item.existing {
        if existing.path != keg.path {
            let _ = keg::link::unlink(cfg, existing, LinkOptions::default());
        }
    }
    let mut link_failed: Option<String> = None;
    if should_link(cfg, item) {
        // A stale record from a previous keg would block the link.
        if cfg.linked_record(item.name()).is_symlink() && !keg.is_linked(cfg) {
            let _ = std::fs::remove_file(cfg.linked_record(item.name()));
        }
        let link_opts = LinkOptions {
            overwrite: opts.overwrite,
            dry_run: false,
            verbose: opts.verbose,
        };
        // The receipt is not there yet, so the aliases come from the formula.
        if let Err(e) = keg::link::link_with_aliases(
            cfg,
            &keg,
            &aliases,
            &item.formula.link_overwrite_paths,
            link_opts,
        ) {
            // `FormulaInstaller#link` reports the conflict and carries on: the
            // keg stays installed, and the run fails at the end.
            link_failed = Some(
                [
                    "The `brew link` step did not complete successfully".to_string(),
                    format!(
                        "The formula built, but is not symlinked into {}",
                        cfg.prefix.display()
                    ),
                    e.to_string(),
                    String::new(),
                    "You can try again using:".to_string(),
                    format!("  brew link {}", item.name()),
                ]
                .join("\n"),
            );
        }
    }

    // 10. Service files.
    if let Err(e) = crate::services::plist::install_service_files(cfg, &item.formula, &keg.path) {
        println!("{e}");
        output::onoe("Failed to install service files");
    }

    // 11-12. `etc`/`var` seeds and the declarative post-install steps.
    if opts.skip_post_install {
        if !opts.quiet {
            output::ohai("Skipping 'post_install' on request");
            println!("You can run it manually using:");
            println!("  brew postinstall {}", item.formula.full_name());
        }
    } else {
        postinstall::install_etc_var(cfg, &keg)?;
        if postinstall::has_post_install(&item.formula)
            && let Err(e) = postinstall::run_post_install(cfg, &item.formula, &keg)
        {
            output::opoo("The post-install step did not complete successfully");
            println!("{e}");
            println!("You can try again using:");
            println!("  brew postinstall {}", item.formula.full_name());
        }
    }

    // 13. The receipt, with the final runtime dependencies.
    //
    // Homebrew's `pour` writes the tab before `finish` links the keg and seeds
    // `etc`/`var`, so a keg whose finishing raised keeps a receipt: the next
    // `brew install` calls it installed and never completes the work. Writing
    // it once, here, is what makes a keg an installation — until then it has
    // no receipt, `discard_unfinished_keg` removes it and a retry redoes the
    // whole install. A failed *link* is not such a failure: it does not stop
    // finishing, so the receipt is written and the keg stays installed, just
    // as it does for Homebrew.
    let runtime = receipt::runtime_dependencies(cfg, index, &item.formula, &|name| {
        plan.planned_version(name)
    });
    let receipt_json = receipt::build(
        cfg,
        receipt::ReceiptArgs {
            formula: &item.formula,
            tab: &tab,
            installed_on_request,
            time: poured_at,
            runtime_dependencies: runtime,
        },
    );
    receipt_json.write(&keg.receipt_path())?;

    // 14. Caveats, printed by the caller with the summary line.
    let caveats = if installed_on_request && !opts.quiet {
        caveats::caveats(cfg, &item.formula, &keg)
    } else {
        caveats::Caveats::default()
    };
    Ok(Outcome {
        caveats,
        link_failed,
    })
}

/// Whether an older keg of this formula was installed on request.
fn previous_on_request(item: &Item) -> bool {
    item.existing
        .iter()
        .filter_map(|k| k.receipt().ok())
        .any(|r| r.installed_on_request)
}

/// `link_keg ||= !formula.keg_only? || auto_link_versioned_keg_only?`, plus the
/// `upgrade` rule that an unlinked keg-only formula stays unlinked.
fn should_link(cfg: &Config, item: &Item) -> bool {
    if !item.formula.is_keg_only() {
        return true;
    }
    // A keg-only formula is relinked when the version it replaces was linked,
    // and a versioned one may be linked on a first install.
    item.was_linked || plan::auto_link_versioned_keg_only(cfg, item)
}

/// Whether a dependency of `item` failed earlier in this run.
///
/// Items are finished in dependency order and a skipped item joins `failed`
/// itself, so checking the direct dependencies propagates transitively.
fn depends_on_failed(item: &Item, failed: &HashSet<String>) -> bool {
    if failed.is_empty() {
        return false;
    }
    item.dependency_names.iter().any(|d| failed.contains(d))
}

fn print_install_header(item: &Item, opts: &InstallOptions, mode: Mode) {
    if opts.quiet {
        return;
    }
    let name = item.formula.full_name();
    if !item.requested {
        let verb = if item.action == Action::Upgrade {
            "Upgrading"
        } else {
            "Installing"
        };
        output::ohai(&format!("{verb} {} dependency: {name}", item.root));
        return;
    }
    match item.action {
        Action::Reinstall => output::ohai(&format!("Reinstalling {name}")),
        // `upgrade` prints its own `==> Upgrading <name>` header with the
        // version transition before calling in here.
        Action::Upgrade if mode.upgrade => {}
        Action::Upgrade => output::ohai(&format!("Upgrading {name}")),
        Action::Install => output::ohai(&format!("Installing {name}")),
    }
}

/// `FormulaInstaller#summary`.
pub fn summary_line(cfg: &Config, keg_path: &std::path::Path, files: u64, bytes: u64) -> String {
    let abv = crate::cli::fmt::abv(files, bytes);
    if cfg.no_emoji {
        format!("{}: {abv}", keg_path.display())
    } else {
        format!("{}  {}: {abv}", cfg.install_badge, keg_path.display())
    }
}

/// `Messages#display_caveats`.
fn print_messages(messages: &[(String, caveats::Caveats)]) {
    let mut notes: Vec<&str> = Vec::new();
    for (_, c) in messages {
        for note in &c.completions_and_elisp {
            if !notes.contains(&note.as_str()) {
                notes.push(note);
            }
        }
    }
    let with_text: Vec<&(String, caveats::Caveats)> =
        messages.iter().filter(|(_, c)| c.text.is_some()).collect();
    if notes.is_empty() && with_text.is_empty() {
        return;
    }
    // `Messages#display_caveats` prints one `==> Caveats` heading, the shared
    // completion notices, then each package's caveats under its own `==> name`
    // heading (`force_caveats: true` makes that happen even for one package).
    output::ohai("Caveats");
    for note in notes {
        println!("{}", note.trim_end_matches('\n'));
    }
    for (name, c) in with_text {
        let Some(text) = &c.text else { continue };
        output::ohai_with(name, text.trim_end_matches('\n'));
    }
}

/// Step 15: `brew cleanup <formula>` unless `HOMEBREW_NO_INSTALL_CLEANUP`.
fn finish_run(cfg: &Config, index: &Index, plan: &Plan, opts: &InstallOptions) -> Result<()> {
    if opts.dry_run || cfg.no_install_cleanup || plan.items.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = plan.items.iter().map(|i| i.name().to_string()).collect();
    cleanup::cleanup_after_install(cfg, index, &names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pluralizes_like_homebrew() {
        assert_eq!(pluralize("formula", 1), "1 formula");
        assert_eq!(pluralize("formula", 3), "3 formulae");
        assert_eq!(pluralize("dependency", 1), "1 dependency");
        assert_eq!(pluralize("dependency", 2), "2 dependencies");
        assert_eq!(pluralize("package", 1), "1 package");
        assert_eq!(pluralize("package", 2), "2 packages");
    }

    #[test]
    fn summary_respects_the_emoji_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_test(tmp.path());
        let keg = std::path::Path::new("/p/Cellar/jq/1.8.2");
        assert_eq!(
            summary_line(&cfg, keg, 20, 1_235_416),
            "🍺  /p/Cellar/jq/1.8.2: 20 files, 1.2MB"
        );
        cfg.install_badge = "🥧".into();
        assert!(summary_line(&cfg, keg, 20, 1_235_416).starts_with("🥧  "));
        cfg.no_emoji = true;
        assert_eq!(
            summary_line(&cfg, keg, 8, 186_010),
            "/p/Cellar/jq/1.8.2: 8 files, 186KB"
        );
    }
}
