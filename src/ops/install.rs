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

/// `Utils.pluralize`, for the handful of stems the install path uses.
pub fn pluralize(stem: &str, count: usize) -> String {
    let suffix = match (stem, count) {
        (_, 1) => "",
        ("formula", _) => "e",
        _ => "s",
    };
    format!("{count} {stem}{suffix}")
}

/// Plan and execute installation of `names` (formula references).
pub fn install_formulae(
    cfg: &Config,
    index: &Index,
    names: &[String],
    opts: &InstallOptions,
) -> Result<()> {
    install_formulae_with(cfg, index, names, opts, Mode::default())
}

/// `install_formulae` with the extra [`Mode`] knobs `upgrade` needs.
pub fn install_formulae_with(
    cfg: &Config,
    index: &Index,
    names: &[String],
    opts: &InstallOptions,
    mode: Mode,
) -> Result<()> {
    let mut roots: Vec<FormulaEntry> = Vec::new();
    for name in names {
        let formula = resolve::resolve_formula(cfg, index, name)?;
        if let Some(message) = plan::check_deprecate_disable(&formula, opts.force)? {
            output::opoo(&message);
        }
        roots.push(formula);
    }

    let plan = build_plan(cfg, index, &roots, opts, mode)?;
    if plan.items.is_empty() {
        // Everything was already installed; the warnings are already printed.
        return finish_run(cfg, index, &plan, opts);
    }

    if opts.dry_run {
        let verb = if opts.reinstall {
            "reinstall"
        } else if mode.upgrade {
            "upgrade"
        } else {
            "install"
        };
        output::ohai(&format!(
            "Would {verb} {}:",
            pluralize("formula", plan.items.len())
        ));
        println!(
            "{}",
            plan.items
                .iter()
                .map(|i| i.name().to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
        return Ok(());
    }

    // 3. Take formula locks for everything in the plan.
    let _locks = take_locks(cfg, &plan)?;

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
    let poured = pour_all(cfg, &plan, &downloads, opts);

    // 8-14. Finish each keg in dependency order.
    let mut failed: HashSet<String> = HashSet::new();
    let mut printed_deps_header = false;
    let mut messages: Vec<(String, caveats::Caveats)> = Vec::new();
    let mut link_failed = false;

    for item in &plan.items {
        if let Some(error) = poured.get(item.name()).and_then(|r| r.as_ref().err()) {
            output::onoe(&format!("{}: {error}", item.name()));
            failed.insert(item.name().to_string());
            continue;
        }
        if depends_on_failed(index, item, &failed) {
            output::onoe(&format!(
                "{}: skipped because a dependency failed to install",
                item.name()
            ));
            failed.insert(item.name().to_string());
            continue;
        }

        if !item.requested && !printed_deps_header {
            printed_deps_header = true;
            if let Some(root) = plan.roots.first() {
                let dep_names: Vec<String> = plan
                    .items
                    .iter()
                    .filter(|i| !i.requested)
                    .map(|i| i.name().to_string())
                    .collect();
                output::ohai(&format!(
                    "Installing dependencies for {}: {}",
                    root,
                    resolve::to_sentence(&dep_names, "and")
                ));
            }
        }
        print_install_header(&plan, item, opts, mode);
        if let Some(download) = downloads.get(item.name()) {
            output::ohai(&format!(
                "Pouring {}",
                download
                    .blob
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            ));
        }

        match finish_item(cfg, index, &plan, item, &downloads, opts) {
            Ok(outcome) => {
                if outcome.link_failed {
                    link_failed = true;
                }
                if !outcome.caveats.is_empty() {
                    messages.push((item.name().to_string(), outcome.caveats));
                }
                let (files, bytes) = keg::disk_usage(&item.keg(cfg).path);
                println!("{}", summary_line(cfg, &item.keg(cfg).path, files, bytes));
            }
            Err(e) => {
                output::onoe(&format!("{}: {e}", item.name()));
                failed.insert(item.name().to_string());
            }
        }
    }

    print_messages(&messages);
    finish_run(cfg, index, &plan, opts)?;

    if !failed.is_empty() {
        return Err(Error::user(format!(
            "Failed to install {}",
            resolve::to_sentence(&failed.into_iter().collect::<Vec<_>>(), "and")
        )));
    }
    if link_failed {
        // The kegs stay installed but the run exits 1, like Homebrew's `ofail`.
        return Err(Error::user(
            "The `brew link` step did not complete successfully".to_string(),
        ));
    }
    Ok(())
}

/// `fetch`: download manifests and blobs only.
pub fn fetch_formulae(
    cfg: &Config,
    index: &Index,
    names: &[String],
    with_deps: bool,
    force: bool,
) -> Result<()> {
    let mut entries: Vec<FormulaEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for name in names {
        let formula = resolve::resolve_formula(cfg, index, name)?;
        if with_deps {
            for dep in deps::recursive_dependency_names(index, &formula.name, DepOptions::default())
            {
                if seen.insert(dep.clone())
                    && let Some(entry) = index.formula(&dep)
                {
                    entries.push(entry);
                }
            }
        }
        if seen.insert(formula.name.clone()) {
            entries.push(formula);
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
#[derive(Debug, Default)]
pub struct Plan {
    pub items: Vec<Item>,
    /// Full names of the formulae the user asked for, in order.
    pub roots: Vec<String>,
}

impl Plan {
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
    let mut wanted: Vec<FormulaEntry> = Vec::new();

    for root in roots {
        let action = if opts.reinstall {
            Action::Reinstall
        } else if mode.upgrade {
            Action::Upgrade
        } else {
            match plan::already_installed(cfg, index, root, opts.only_dependencies) {
                Already::NotInstalled => Action::Install,
                Already::Outdated => Action::Upgrade,
                Already::UpToDate { message } | Already::NotLinked { message } => {
                    if !opts.quiet {
                        output::opoo(&message);
                    }
                    // Homebrew still records the explicit request.
                    mark_installed_on_request(cfg, &root.name);
                    continue;
                }
            }
        };
        plan::check_conflicts(cfg, root)?;
        wanted.push(root.clone());
        if opts.only_dependencies {
            continue;
        }
        plan.items.push(make_item(
            cfg,
            root,
            action,
            true,
            opts.on_request || opts.reinstall,
        )?);
    }

    // Dependencies go first, in install order.
    let closure = plan::dependency_closure(cfg, index, &wanted, opts.ignore_dependencies);
    let mut dependency_items = Vec::new();
    for entry in closure {
        let installed = !keg::installed_kegs(cfg, &entry.name).is_empty();
        let outdated = installed && plan::is_outdated(cfg, index, &entry.name);
        if installed && !outdated {
            continue;
        }
        if installed && outdated && cfg.no_install_upgrade {
            continue;
        }
        let action = if outdated {
            Action::Upgrade
        } else {
            Action::Install
        };
        // `installed_on_request` stays true when an existing receipt says so.
        let on_request = deps::installed_on_request(cfg, &entry.name);
        dependency_items.push(make_item(cfg, &entry, action, false, on_request)?);
    }
    let mut items = dependency_items;
    items.append(&mut plan.items);
    plan.items = items;
    Ok(plan)
}

fn make_item(
    cfg: &Config,
    formula: &FormulaEntry,
    action: Action,
    requested: bool,
    installed_on_request: bool,
) -> Result<Item> {
    let bottle = plan::require_bottle(cfg, formula)?;
    let existing = keg::installed_kegs(cfg, &formula.name);
    let was_linked = existing.iter().any(|k| k.is_linked(cfg));
    Ok(Item {
        cellar: formula.bottle_cellar_kind(),
        formula: formula.clone(),
        bottle,
        action,
        requested,
        installed_on_request,
        existing,
        was_linked,
    })
}

/// `install/check.rb`: a formula the user asked for but that is already there
/// still stops being "installed as a dependency".
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

fn take_locks(cfg: &Config, plan: &Plan) -> Result<Vec<Lock>> {
    let mut names: Vec<&str> = plan.items.iter().map(Item::name).collect();
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
    let deps: Vec<String> = plan
        .items
        .iter()
        .filter(|i| !i.requested)
        .map(|i| i.name().to_string())
        .collect();
    if !deps.is_empty()
        && !opts.quiet
        && let Some(root) = plan.roots.first()
    {
        output::ohai(&format!(
            "Fetching dependencies for {root}: {}",
            resolve::to_sentence(&deps, "and")
        ));
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

/// Extract and relocate every keg; independent formulae run in parallel.
fn pour_all(
    cfg: &Config,
    plan: &Plan,
    downloads: &HashMap<String, Download>,
    opts: &InstallOptions,
) -> HashMap<String, Result<()>> {
    // `reinstall` and `upgrade` replace a keg that may still be linked.
    for item in &plan.items {
        if item.action == Action::Reinstall {
            for existing in &item.existing {
                if existing.version.to_string() == item.pkg_version() {
                    let _ = keg::link::unlink(cfg, existing, LinkOptions::default());
                }
            }
        }
    }

    let results: Vec<(String, Result<()>)> = plan
        .items
        .par_iter()
        .map(|item| {
            let name = item.name().to_string();
            let Some(download) = downloads.get(&name) else {
                return (name, Err(Error::user("no download for this formula")));
            };
            (name, pour_one(cfg, item, download, opts))
        })
        .collect();
    results.into_iter().collect()
}

fn pour_one(cfg: &Config, item: &Item, download: &Download, _opts: &InstallOptions) -> Result<()> {
    let pkg_version = item.pkg_version();
    let replace = item.action == Action::Reinstall;
    let keg_path =
        extract::extract_bottle(cfg, &download.blob, item.name(), &pkg_version, replace)?;

    let tab = receipt::tab_with_keg_fallback(&download.manifest.tab, &keg_path);
    let openjdk = openjdk_dependency(&tab);
    let result = relocate::relocate_keg(
        cfg,
        relocate::RelocateArgs {
            keg_path: &keg_path,
            cellar_kind: &item.cellar,
            tab: &tab,
            openjdk_dep: openjdk.as_deref(),
        },
    );
    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            // `FormulaInstaller#install` removes a keg that failed to pour.
            let _ = std::fs::remove_dir_all(&keg_path);
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
    link_failed: bool,
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

    // 6. Receipt, with the runtime dependencies known so far.
    let installed_on_request = match item.action {
        // `reinstall`/`upgrade` keep the old receipt's flag.
        Action::Reinstall | Action::Upgrade => {
            item.installed_on_request || previous_on_request(item)
        }
        Action::Install => item.installed_on_request,
    };
    let runtime = receipt::runtime_dependencies(cfg, index, &item.formula, &|name| {
        plan.planned_version(name)
    });
    let mut receipt_json = receipt::build(
        cfg,
        receipt::ReceiptArgs {
            formula: &item.formula,
            tab: &tab,
            installed_on_request,
            time: receipt::now(),
            runtime_dependencies: runtime,
        },
    );
    receipt_json.write(&keg.receipt_path())?;

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
    let mut link_failed = false;
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
        if let Err(e) = keg::link::link(cfg, &keg, &item.formula.link_overwrite_paths, link_opts) {
            output::onoe("The `brew link` step did not complete successfully");
            println!(
                "The formula built, but is not symlinked into {}",
                cfg.prefix.display()
            );
            println!("{e}");
            println!();
            println!("You can try again using:");
            println!("  brew link {}", item.name());
            link_failed = true;
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

    // 13. Rewrite the receipt with the final runtime dependencies.
    let runtime = receipt::runtime_dependencies(cfg, index, &item.formula, &|name| {
        plan.planned_version(name)
    });
    receipt_json.runtime_dependencies = Some(runtime);
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

fn depends_on_failed(index: &Index, item: &Item, failed: &HashSet<String>) -> bool {
    if failed.is_empty() {
        return false;
    }
    deps::recursive_dependency_names(index, item.name(), DepOptions::default())
        .iter()
        .any(|d| failed.contains(d))
}

fn print_install_header(plan: &Plan, item: &Item, opts: &InstallOptions, mode: Mode) {
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
        let root = plan.roots.first().cloned().unwrap_or_default();
        output::ohai(&format!("{verb} {root} dependency: {name}"));
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
    output::ohai("Caveats");
    for note in notes {
        println!("{}", note.trim_end_matches('\n'));
    }
    for (name, c) in with_text {
        let Some(text) = &c.text else { continue };
        if messages.len() == 1 {
            println!("{}", text.trim_end_matches('\n'));
        } else {
            output::ohai_with(name, text.trim_end_matches('\n'));
        }
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
