//! Dependency graphs over the index and the installed kegs.
//!
//! Semantics (port of `Formula#recursive_dependencies`, `deps`, `uses`,
//! `leaves`, `Utils::Autoremove`):
//! - Runtime deps are `stable_dependencies` minus `:build`/`:test`-only ones.
//!   `:optional` deps are excluded unless `include_optional`; `:recommended`
//!   included unless `skip_recommended`.
//! - `uses_from_macos` entries count as dependencies only on Linux, or on
//!   macOS when the running version is older than `since`.
//! - Order: post-order depth-first (dependencies before dependents),
//!   deduplicated, stable with respect to declaration order.
//! - `uses` inverts the graph over the whole index (or over installed kegs'
//!   receipts when `installed`).
//! - `leaves`: installed formulae not listed in any installed keg's
//!   `runtime_dependencies` nor required by an installed cask.
//! - autoremove set: installed formulae with `installed_on_request == false`
//!   that no installed formula or cask depends on, computed to a fixed point.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::api::index::{Index, depflag};
use crate::config::Config;
use crate::error::Result;
use crate::model::{Dependency, DependencyTag, FormulaEntry};
use crate::platform::Host;

#[derive(Debug, Clone, Copy, Default)]
pub struct DepOptions {
    pub include_build: bool,
    pub include_test: bool,
    pub include_optional: bool,
    pub skip_recommended: bool,
    pub include_implicit: bool,
}

impl DepOptions {
    /// Whether a dependency with these tags survives `select_includes`.
    ///
    /// `:test?` is only honored for the root dependent, matching
    /// `DependenciesHelpers.recursive_includes`.
    fn keeps(&self, flags: u8, is_root: bool) -> bool {
        let has = |bit: u8| flags & bit != 0;
        if self.skip_recommended && has(depflag::RECOMMENDED) {
            return false;
        }
        let tagged = has(depflag::BUILD)
            || has(depflag::TEST)
            || has(depflag::OPTIONAL)
            || has(depflag::RECOMMENDED)
            || has(depflag::IMPLICIT);
        if !tagged || has(depflag::RECOMMENDED) {
            return true;
        }
        (has(depflag::IMPLICIT) && self.include_implicit)
            || (has(depflag::BUILD) && self.include_build)
            || (has(depflag::TEST) && self.include_test && is_root)
            || (has(depflag::OPTIONAL) && self.include_optional)
    }
}

fn tags_to_flags(dep: &Dependency) -> u8 {
    let mut f = 0u8;
    for t in &dep.tags {
        f |= match t {
            DependencyTag::Build => depflag::BUILD,
            DependencyTag::Test => depflag::TEST,
            DependencyTag::Optional => depflag::OPTIONAL,
            DependencyTag::Recommended => depflag::RECOMMENDED,
            DependencyTag::Implicit => depflag::IMPLICIT,
        };
    }
    f
}

/// Whether the host still needs a `uses_from_macos` dependency: always on
/// Linux, and on macOS only below the `since:` release.
pub fn uses_from_macos_is_dependency(since_major: u32) -> bool {
    let host = Host::detect();
    if host.linux {
        return true;
    }
    match host.macos {
        Some(v) => since_major != 0 && v.major < since_major,
        None => false,
    }
}

/// Direct dependencies of `formula` after applying `opts` and the
/// `uses_from_macos` platform rule, read from the entry's own stanzas.
///
/// This is the only source for a tap formula, which has no row in the packed
/// core index; `entry_dependency_names` picks it for those.
pub fn direct_dependencies(
    formula: &FormulaEntry,
    opts: DepOptions,
    is_root: bool,
) -> Vec<Dependency> {
    let mut out = Vec::new();
    for dep in formula.dependencies() {
        if opts.keeps(tags_to_flags(&dep), is_root) {
            out.push(dep);
        }
    }
    for ufm in formula.uses_from_macos() {
        let since = ufm
            .since
            .as_deref()
            .and_then(crate::platform::MacOsVersion::major_for_symbol)
            .unwrap_or(0);
        if !uses_from_macos_is_dependency(since) {
            continue;
        }
        if opts.keeps(tags_to_flags(&ufm.dep), is_root) {
            out.push(ufm.dep);
        }
    }
    out
}

/// Direct dependency references of a *resolved* entry.
///
/// A core formula uses the index's compact records (no JSON parsing); a tap
/// formula — or one rebuilt from a receipt — carries its own `depends_on`
/// list, which the index knows nothing about. References keep whatever
/// qualification the formula wrote, so `depends_on "user/repo/x"` survives.
pub fn entry_dependency_names(
    index: &Index,
    entry: &FormulaEntry,
    opts: DepOptions,
    is_root: bool,
) -> Vec<String> {
    if crate::resolve::is_core_tap(&entry.tap) && index.has_formula(&entry.name) {
        return direct_dependency_names(index, &entry.name, opts, is_root);
    }
    direct_dependencies(entry, opts, is_root)
        .into_iter()
        .map(|d| d.name)
        .collect()
}

/// Direct dependency names read straight from the index's compact records.
pub fn direct_dependency_names(
    index: &Index,
    name: &str,
    opts: DepOptions,
    is_root: bool,
) -> Vec<String> {
    index
        .formula_deps(name)
        .into_iter()
        .filter(|(_, flags, since)| {
            if flags & depflag::USES_FROM_MACOS != 0
                && !uses_from_macos_is_dependency(*since as u32)
            {
                return false;
            }
            opts.keeps(*flags, is_root)
        })
        .map(|(n, _, _)| n.to_string())
        .collect()
}

/// Transitive dependency names in install order (dependencies first).
pub fn recursive_dependency_names(index: &Index, root: &str, opts: DepOptions) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    visit(index, root, opts, true, &mut seen, &mut order, &mut stack);
    order
}

fn visit(
    index: &Index,
    name: &str,
    opts: DepOptions,
    is_root: bool,
    seen: &mut HashSet<String>,
    order: &mut Vec<String>,
    stack: &mut Vec<String>,
) {
    if stack.iter().any(|s| s == name) {
        return; // circular
    }
    stack.push(name.to_string());
    for dep in direct_dependency_names(index, name, opts, is_root) {
        visit(index, &dep, opts, false, seen, order, stack);
        if seen.insert(dep.clone()) {
            order.push(dep);
        }
    }
    stack.pop();
}

/// Full transitive closure in install order (dependencies first), starting
/// from a *resolved* entry.
///
/// Every dependency reference is resolved through
/// [`crate::resolve::resolve_dependency`], so a tap formula's dependencies are
/// found in the core API, in its own tap or in another installed tap, and a
/// `user/repo/name` reference selects that tap. A required dependency that
/// resolves nowhere is a `FormulaUnavailableError` naming the dependent
/// (`Formula#recursive_dependencies`), never a silent omission.
pub fn recursive_dependencies(
    cfg: &Config,
    index: &Index,
    formula: &FormulaEntry,
    opts: DepOptions,
) -> Result<Vec<FormulaEntry>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut order: Vec<FormulaEntry> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    visit_entry(
        cfg, index, formula, opts, true, &mut seen, &mut order, &mut stack,
    )?;
    Ok(order)
}

#[allow(clippy::too_many_arguments)]
fn visit_entry(
    cfg: &Config,
    index: &Index,
    entry: &FormulaEntry,
    opts: DepOptions,
    is_root: bool,
    seen: &mut HashSet<String>,
    order: &mut Vec<FormulaEntry>,
    stack: &mut Vec<String>,
) -> Result<()> {
    let full = entry.full_name();
    if stack.contains(&full) {
        return Ok(()); // circular
    }
    stack.push(full);
    for reference in entry_dependency_names(index, entry, opts, is_root) {
        let dep = crate::resolve::resolve_dependency(cfg, index, &reference, entry)?;
        visit_entry(cfg, index, &dep, opts, false, seen, order, stack)?;
        if seen.insert(dep.full_name()) {
            order.push(dep);
        }
    }
    stack.pop();
    Ok(())
}

/// The same walk as [`recursive_dependencies`], but a reference that resolves
/// nowhere is reported as itself instead of failing the walk.
///
/// The read-only listings (`deps`, `uses`, `info`) show what a formula
/// declares even when part of it is unavailable; refusing to guess is the
/// install plan's job.
pub fn recursive_dependency_references(
    cfg: &Config,
    index: &Index,
    formula: &FormulaEntry,
    opts: DepOptions,
) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    walk_references(
        cfg, index, formula, opts, true, &mut seen, &mut order, &mut stack,
    );
    order
}

#[allow(clippy::too_many_arguments)]
fn walk_references(
    cfg: &Config,
    index: &Index,
    entry: &FormulaEntry,
    opts: DepOptions,
    is_root: bool,
    seen: &mut HashSet<String>,
    order: &mut Vec<String>,
    stack: &mut Vec<String>,
) {
    let full = entry.full_name();
    if stack.contains(&full) {
        return; // circular
    }
    stack.push(full);
    for reference in entry_dependency_names(index, entry, opts, is_root) {
        match crate::resolve::resolve_dependency(cfg, index, &reference, entry) {
            Ok(dep) => {
                walk_references(cfg, index, &dep, opts, false, seen, order, stack);
                if seen.insert(dep.name.clone()) {
                    order.push(dep.name);
                }
            }
            Err(_) => {
                let name = short_name(&reference).to_string();
                if seen.insert(name.clone()) {
                    order.push(name);
                }
            }
        }
    }
    stack.pop();
}

/// Formulae (from the index or installed set) that depend on `name`.
///
/// An installed formula is inverted through the entry its receipt's tap
/// resolves to, so a keg poured from a third-party tap contributes the
/// dependencies that tap declares rather than the core formula's.
pub fn uses(
    cfg: &Config,
    index: &Index,
    name: &str,
    recursive: bool,
    installed_only: bool,
    opts: DepOptions,
) -> Result<Vec<String>> {
    let candidates: Vec<String> = if installed_only {
        crate::keg::installed_formula_names(cfg)
    } else {
        index.formula_names()
    };

    let mut out: Vec<String> = candidates
        .into_iter()
        .filter(|candidate| {
            if candidate == name {
                return false;
            }
            // An installed candidate may not be the core formula of that
            // name; resolve it before inverting its graph.
            if installed_only
                && let Some(tap) = crate::resolve::installed_tap(cfg, candidate)
                && let Ok(entry) =
                    crate::resolve::resolve_formula(cfg, index, &format!("{tap}/{candidate}"))
            {
                return entry_depends_on(cfg, index, &entry, name, recursive, opts);
            }
            if recursive {
                recursive_dependency_names(index, candidate, opts)
                    .iter()
                    .any(|d| d == name)
            } else {
                direct_dependency_names(index, candidate, opts, true)
                    .iter()
                    .any(|d| d == name)
            }
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// Whether `entry` depends on the formula named `name` (by rack name).
fn entry_depends_on(
    cfg: &Config,
    index: &Index,
    entry: &FormulaEntry,
    name: &str,
    recursive: bool,
    opts: DepOptions,
) -> bool {
    if recursive {
        recursive_dependency_references(cfg, index, entry, opts)
            .iter()
            .any(|d| d == name)
    } else {
        entry_dependency_names(index, entry, opts, true)
            .iter()
            .any(|d| short_name(d) == name)
    }
}

/// Tokens of every cask staged in the Caskroom.
pub fn installed_cask_tokens(cfg: &Config) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(cfg.caskroom()) else {
        return vec![];
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if name.starts_with('.') || !e.path().is_dir() {
                return None;
            }
            Some(name)
        })
        .collect();
    out.sort();
    out
}

/// Every formula name some installed keg or cask records as a runtime dependency.
fn installed_dependency_names(cfg: &Config, index: &Index) -> HashSet<String> {
    let mut names = HashSet::new();
    for formula in crate::keg::installed_formula_names(cfg) {
        let Some(keg) = crate::keg::latest_keg(cfg, &formula) else {
            continue;
        };
        let receipt = keg.receipt().ok();
        let recorded: Vec<String> = receipt
            .as_ref()
            .map(|r| {
                r.runtime_dependency_names()
                    .into_iter()
                    .map(|full| short_name(full).to_string())
                    .collect()
            })
            .unwrap_or_default();
        if recorded.is_empty() {
            // No receipt (or an empty one): fall back to the API graph.
            for dep in recursive_dependency_names(index, &formula, DepOptions::default()) {
                names.insert(dep);
            }
        } else {
            names.extend(recorded);
        }
    }
    for token in installed_cask_tokens(cfg) {
        if let Some(cask) = index.cask(&token) {
            for dep in cask.formula_dependencies() {
                names.insert(short_name(&dep).to_string());
            }
        }
    }
    names
}

/// `Utils.name_from_full_name`: the part after the last `/`.
pub fn short_name(full: &str) -> &str {
    full.rsplit('/').next().unwrap_or(full)
}

/// Every name an installed formula can be referred to by (`possible_names`).
fn possible_names(index: &Index, name: &str) -> Vec<String> {
    let mut names = vec![name.to_string()];
    if let Some(entry) = index.formula(name) {
        names.extend(entry.oldnames.iter().cloned());
        names.extend(entry.aliases.iter().cloned());
    }
    names
}

/// Installed formulae nothing else installed depends on.
pub fn leaves(cfg: &Config) -> Result<Vec<String>> {
    let tag = Host::detect().bottle_tag();
    let index = Index::load(cfg, &tag)?;
    leaves_with_index(cfg, &index)
}

pub fn leaves_with_index(cfg: &Config, index: &Index) -> Result<Vec<String>> {
    let deps = installed_dependency_names(cfg, index);
    let mut out: Vec<String> = crate::keg::installed_formula_names(cfg)
        .into_iter()
        .filter(|name| !possible_names(index, name).iter().any(|n| deps.contains(n)))
        .collect();
    out.sort();
    Ok(out)
}

/// Whether an installed formula was installed because the user asked for it.
pub fn installed_on_request(cfg: &Config, name: &str) -> bool {
    crate::keg::installed_kegs(cfg, name).iter().any(|keg| {
        keg.receipt()
            .map(|r| r.installed_on_request)
            .unwrap_or(false)
    })
}

/// Formulae `autoremove` would uninstall.
///
/// Port of `Utils::Autoremove.removable_formulae`: start from the installed
/// formulae that were not installed on request, then repeatedly drop any that
/// something still-kept depends on until the set stops shrinking.
pub fn removable(cfg: &Config, index: &Index) -> Result<BTreeSet<String>> {
    let installed = crate::keg::installed_formula_names(cfg);
    let mut removable: BTreeSet<String> = installed
        .iter()
        .filter(|n| !installed_on_request(cfg, n))
        .cloned()
        .collect();

    // Direct runtime dependencies of every installed formula and cask.
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    for name in &installed {
        let deps: Vec<String> =
            match crate::keg::latest_keg(cfg, name).and_then(|k| k.receipt().ok()) {
                Some(r) if !r.runtime_dependency_names().is_empty() => r
                    .runtime_dependency_names()
                    .into_iter()
                    .map(|f| short_name(f).to_string())
                    .collect(),
                _ => direct_dependency_names(index, name, DepOptions::default(), true),
            };
        edges.insert(name.clone(), deps);
    }
    let cask_deps: Vec<String> = installed_cask_tokens(cfg)
        .iter()
        .filter_map(|t| index.cask(t))
        .flat_map(|c| c.formula_dependencies())
        .map(|d| short_name(&d).to_string())
        .collect();

    loop {
        let mut needed: HashSet<&str> = HashSet::new();
        for (name, deps) in &edges {
            if removable.contains(name) {
                continue;
            }
            for d in deps {
                needed.insert(d.as_str());
            }
        }
        for d in &cask_deps {
            needed.insert(d.as_str());
        }
        let before = removable.len();
        removable.retain(|n| {
            !possible_names(index, n)
                .iter()
                .any(|alias| needed.contains(alias.as_str()))
        });
        if removable.len() == before {
            break;
        }
    }
    Ok(removable)
}

/// Installed formulae whose recorded runtime dependencies are not installed
/// (`brew missing`).
pub fn missing(cfg: &Config, index: &Index, names: &[String]) -> Vec<(String, Vec<String>)> {
    let installed: HashSet<String> = crate::keg::installed_formula_names(cfg)
        .into_iter()
        .collect();
    let targets: Vec<String> = if names.is_empty() {
        let mut v: Vec<String> = installed.iter().cloned().collect();
        v.sort();
        v
    } else {
        names.to_vec()
    };
    targets
        .into_iter()
        .filter_map(|name| {
            let deps = recursive_dependency_names(index, &name, DepOptions::default());
            let mut gone: Vec<String> = deps
                .into_iter()
                .filter(|d| !installed.contains(d))
                .filter(|d| index.has_formula(d))
                .collect();
            gone.sort();
            gone.dedup();
            (!gone.is_empty()).then_some((name, gone))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_rules() {
        let plain = DepOptions::default();
        assert!(plain.keeps(0, true));
        assert!(!plain.keeps(depflag::BUILD, true));
        assert!(!plain.keeps(depflag::TEST, true));
        assert!(!plain.keeps(depflag::OPTIONAL, true));
        assert!(plain.keeps(depflag::RECOMMENDED, true));

        let build = DepOptions {
            include_build: true,
            ..Default::default()
        };
        assert!(build.keeps(depflag::BUILD, true));
        assert!(build.keeps(depflag::BUILD | depflag::TEST, true));

        let test = DepOptions {
            include_test: true,
            ..Default::default()
        };
        assert!(test.keeps(depflag::TEST, true));
        assert!(!test.keeps(depflag::TEST, false), "test deps are root-only");

        let skip = DepOptions {
            skip_recommended: true,
            ..Default::default()
        };
        assert!(!skip.keeps(depflag::RECOMMENDED, true));
    }

    #[test]
    fn short_names() {
        assert_eq!(short_name("oniguruma"), "oniguruma");
        assert_eq!(short_name("user/repo/foo"), "foo");
    }
}
