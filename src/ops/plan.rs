//! Install planning: what `install`, `reinstall` and `upgrade` will actually do.
//!
//! Ports the decisions `FormulaInstaller#prelude`, `#compute_dependencies`,
//! `#check_conflicts` and `Homebrew::Install.install_formula?` make before any
//! download starts:
//!
//! - refuse `disable!`d formulae and warn about `deprecate!`d ones
//!   (`DeprecateDisable.message`),
//! - refuse a bottle whose `pour_bottle only_if:` is unsatisfied, and delegate
//!   when there is no bottle for this platform,
//! - expand the runtime-dependency closure, dropping satisfied dependencies and
//!   upgrading outdated ones,
//! - decide, per requested formula, between installing, upgrading in place and
//!   printing one of Homebrew's "already installed" warnings.

use crate::api::index::Index;
use crate::bottle::{BottleRef, relocate};
use crate::config::Config;
use crate::deps::{self, DepOptions};
use crate::error::{Error, Result};
use crate::keg::{self, Keg};
use crate::model::FormulaEntry;
use crate::model::formula::{BottleCellar, DeprecateDisable};
use crate::platform::{BottleTag, Host};
use crate::version::PkgVersion;

/// What to do with one formula named on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Install,
    /// Installed but outdated, and `HOMEBREW_NO_INSTALL_UPGRADE` is unset.
    Upgrade,
    /// Installed at the requested version; `reinstall` replaces it.
    Reinstall,
}

/// One formula in the plan, with everything the pour needs.
#[derive(Debug, Clone)]
pub struct Item {
    pub formula: FormulaEntry,
    pub bottle: BottleRef,
    pub cellar: BottleCellar,
    pub action: Action,
    /// False for dependencies pulled in implicitly.
    pub requested: bool,
    pub installed_on_request: bool,
    /// Kegs of this formula that the new one replaces.
    pub existing: Vec<Keg>,
    /// Whether one of `existing` was linked before this run started.
    pub was_linked: bool,
}

impl Item {
    pub fn name(&self) -> &str {
        &self.formula.name
    }

    pub fn pkg_version(&self) -> String {
        self.formula.pkg_version()
    }

    pub fn keg(&self, cfg: &Config) -> Keg {
        Keg::new(cfg, &self.formula.name, &self.pkg_version())
    }
}

/// `DeprecateDisable.message`, with the leading `<full_name> has been ` added by
/// the caller.
pub fn deprecate_disable_message(formula: &FormulaEntry) -> Option<String> {
    let (kind, info) = if let Some(d) = formula.disablement() {
        ("disabled", d)
    } else {
        ("deprecated", formula.deprecation()?)
    };
    let mut message = match info.because.as_deref() {
        Some(reason) if !reason.is_empty() => {
            format!("{kind} because it {}!", humanize_reason(reason))
        }
        _ => format!("{kind}!"),
    };
    // Homebrew derives a disable date from the deprecation date when the
    // formula has not named one; the internal API carries only one date per
    // stanza, so it is used as is.
    if let Some(date) = info.date.as_deref().filter(|d| !d.is_empty()) {
        if kind == "disabled" {
            message.push_str(&format!(" It was disabled on {date}."));
        } else {
            message.push_str(&format!(" It will be disabled on {date}."));
        }
    }
    if let Some(replacement) = replacement_with_type(&info) {
        message.push_str(&format!("\nReplacement:\n  brew install {replacement}\n"));
    }
    Some(message)
}

fn replacement_with_type(info: &DeprecateDisable) -> Option<String> {
    match (
        info.replacement_formula.as_deref(),
        info.replacement_cask.as_deref(),
    ) {
        (Some(f), Some(c)) if f == c => Some(f.to_string()),
        (Some(f), _) => Some(format!("--formula {f}")),
        (None, Some(c)) => Some(format!("--cask {c}")),
        (None, None) => None,
    }
}

/// `DeprecateDisable::FORMULA_DEPRECATE_DISABLE_REASONS`.
fn humanize_reason(reason: &str) -> String {
    match reason {
        "does_not_build" => "does not build".into(),
        "no_license" => "has no license".into(),
        "repo_archived" => "has an archived upstream repository".into(),
        "repo_removed" => "has a removed upstream repository".into(),
        "unmaintained" => "is not maintained upstream".into(),
        "unreachable" => "is no longer reliably reachable upstream".into(),
        "unsupported" => "is not supported upstream".into(),
        "deprecated_upstream" => "is deprecated upstream".into(),
        "versioned_formula" => "is a versioned formula".into(),
        "checksum_mismatch" => "was built with an initially released source file that had \
             a different checksum than the current one. \
             Upstream's repository might have been compromised. \
             We can re-package this once upstream has confirmed that they retagged their release"
            .into(),
        other => other.to_string(),
    }
}

/// `FormulaInstaller#prelude_fetch`: refuse a disabled formula, warn about a
/// deprecated one.
pub fn check_deprecate_disable(formula: &FormulaEntry, force: bool) -> Result<Option<String>> {
    let Some(message) = deprecate_disable_message(formula) else {
        return Ok(None);
    };
    let message = format!("{} has been {message}", formula.full_name());
    if formula.is_disabled() && !force {
        return Err(Error::user(message));
    }
    Ok(Some(message))
}

/// `Formula#pour_bottle?` for `only_if:` conditions the API records.
///
/// Returns the unsatisfied reason (`pour_bottle_check_unsatisfied_reason`), or
/// `None` when the bottle may be poured.
pub fn pour_bottle_unsatisfied_reason(cfg: &Config, formula: &FormulaEntry) -> Option<String> {
    match formula.pour_bottle_only_if().as_deref() {
        Some("default_prefix") if !cfg.is_default_prefix() => Some(format!(
            "The bottle needs to be installed into {}.",
            Host::detect().bottle_tag().default_prefix()
        )),
        Some("clt_installed")
            if !std::path::Path::new("/Library/Developer/CommandLineTools").exists() =>
        {
            Some("The bottle needs the Apple Command Line Tools to be installed.".to_string())
        }
        _ => None,
    }
}

/// The bottle reference for a formula, or `None` when it has none for this tag.
pub fn bottle_for(cfg: &Config, formula: &FormulaEntry) -> Option<BottleRef> {
    let sha256 = formula.bottle_checksum.clone()?;
    let tag = match formula.bottle_tag.as_deref() {
        Some(t) => BottleTag::parse(t)?,
        None => Host::detect().bottle_tag(),
    };
    Some(BottleRef {
        name: formula.name.clone(),
        pkg_version: formula.pkg_version(),
        rebuild: formula.bottle_rebuild,
        tag,
        root_url: formula
            .bottle_root_url
            .clone()
            .unwrap_or_else(|| cfg.bottle_domain.clone()),
        sha256,
    })
}

/// `FormulaInstaller#pour_bottle?`, minus the build-time options fastbrew does
/// not support. An `Err(NeedsDelegation)` means the Ruby `brew` must take over.
pub fn require_bottle(cfg: &Config, formula: &FormulaEntry) -> Result<BottleRef> {
    if let Some(reason) = pour_bottle_unsatisfied_reason(cfg, formula) {
        return Err(Error::NeedsDelegation {
            reason: format!("{}: {}", formula.name, lowercase_first(&reason)),
        });
    }
    bottle_for(cfg, formula).ok_or_else(|| Error::NeedsDelegation {
        reason: format!("{}: no bottle available!", formula.name),
    })
}

fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Whether an installed keg already satisfies `formula`
/// (`Dependency#satisfied?`: the latest version is installed).
pub fn is_satisfied(cfg: &Config, formula: &FormulaEntry) -> bool {
    let wanted = PkgVersion::parse(&formula.pkg_version());
    keg::installed_kegs(cfg, &formula.name)
        .iter()
        .any(|k| k.version == wanted)
}

/// `Formula#outdated?` for a dependency already on disk.
pub fn is_outdated(cfg: &Config, index: &Index, name: &str) -> bool {
    !crate::ops::outdated::outdated_kegs(cfg, index, name).is_empty()
}

/// `FormulaInstaller#check_conflicts`: refuse to install when a formula this
/// one `conflicts_with` is installed *and* linked.
pub fn check_conflicts(cfg: &Config, formula: &FormulaEntry) -> Result<()> {
    let mut conflicts: Vec<(String, Option<String>)> = Vec::new();
    for (name, reason) in formula.conflicts_with() {
        if name == formula.name || name == formula.full_name() {
            continue;
        }
        let linked = cfg.linked_record(&name).exists() && cfg.opt_record(&name).exists();
        if linked {
            conflicts.push((name, reason));
        }
    }
    if conflicts.is_empty() {
        return Ok(());
    }
    Err(Error::user(conflict_message(
        cfg,
        &formula.full_name(),
        &conflicts,
    )))
}

/// `FormulaConflictError#message` (`exceptions.rb`).
pub fn conflict_message(
    cfg: &Config,
    full_name: &str,
    conflicts: &[(String, Option<String>)],
) -> String {
    let mut lines = vec![format!(
        "Cannot install {full_name} because conflicting formulae are installed."
    )];
    for (other, reason) in conflicts {
        lines.push(match reason {
            Some(r) => format!("  {other}: because {r}"),
            None => format!("  {other}"),
        });
    }
    lines.push(String::new());
    lines.push(format!(
        "Please `brew unlink {}` before continuing.\n\n\
         Unlinking removes a formula's symlinks from {}. You can\n\
         link the formula again after the install finishes. You can `--force` this\n\
         install, but the build may fail or cause obscure side effects in the\n\
         resulting software.\n",
        conflicts
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        cfg.prefix.display()
    ));
    lines.join("\n")
}

/// Refuse a fixed-cellar bottle our prefix is too long for.
pub fn check_relocatable(
    cfg: &Config,
    formula: &FormulaEntry,
    tab: &crate::bottle::BottleTab,
) -> Result<()> {
    let cellar = formula.bottle_cellar_kind();
    if relocate::compatible_locations(cfg, &cellar, tab) {
        return Ok(());
    }
    Err(Error::NeedsDelegation {
        reason: relocate::incompatible_locations_message(cfg, &formula.name, &cellar, tab),
    })
}

/// How a request for an already-installed formula is handled
/// (`Homebrew::Install.install_formula?`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Already {
    /// Nothing relevant is installed: go ahead.
    NotInstalled,
    /// Installed and outdated, and nothing blocks the upgrade.
    Outdated,
    /// Installed already: print `notice` and skip.
    Installed { notice: Notice },
}

/// Which of Homebrew's reporters prints the "already installed" text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// `opoo`: a warning on stderr.
    Warning(String),
    /// `onoe`: an error on stderr that does not by itself fail the run.
    Error(String),
}

impl Notice {
    pub fn message(&self) -> &str {
        match self {
            Notice::Warning(m) | Notice::Error(m) => m,
        }
    }

    /// Print it the way `install/check.rb` does.
    pub fn print(&self) {
        match self {
            Notice::Warning(m) => crate::output::opoo(m),
            Notice::Error(m) => crate::output::onoe(m),
        }
    }
}

/// Version recorded in `var/homebrew/linked/<name>`, the only thing
/// `Formula#linked?` and `#linked_version` look at.
pub fn linked_version(cfg: &Config, name: &str) -> Option<String> {
    let record = cfg.linked_record(name);
    if !record.is_symlink() {
        return None;
    }
    let target = std::fs::read_link(&record).ok()?;
    Some(target.file_name()?.to_str()?.to_string())
}

/// Version `opt/<name>` points at (`Keg.for(formula.opt_prefix).version`).
fn optlinked_version(cfg: &Config, name: &str) -> Option<String> {
    let record = cfg.opt_record(name);
    if !record.is_symlink() {
        return None;
    }
    let target = std::fs::read_link(&record).ok()?;
    Some(target.file_name()?.to_str()?.to_string())
}

/// Decide what to print (and whether to install) for a requested formula that
/// may already be installed. Port of `Homebrew::Install.install_formula?`,
/// restricted to the stable-bottle cases fastbrew handles.
pub fn already_installed(
    cfg: &Config,
    index: &Index,
    formula: &FormulaEntry,
    only_dependencies: bool,
) -> Already {
    let name = &formula.name;
    let kegs = keg::installed_kegs(cfg, name);
    if kegs.is_empty() {
        return Already::NotInstalled;
    }
    let full_name = formula.full_name();
    let pkg_version = formula.pkg_version();
    let pinned = keg::is_pinned(cfg, name);
    let outdated = is_outdated(cfg, index, name);
    let unpin = if pinned {
        format!("brew unpin {full_name} && ")
    } else {
        String::new()
    };

    // A keg-only formula is checked against its `opt` record first: installing
    // a second version silently would break everything linked against it.
    if formula.is_keg_only() {
        let optlinked = optlinked_version(cfg, name);
        if let Some(optlinked) = optlinked {
            if outdated {
                if !cfg.no_install_upgrade && !pinned {
                    return Already::Outdated;
                }
                return Already::Installed {
                    notice: Notice::Error(format!(
                        "{full_name} {optlinked} is already installed.\nTo upgrade to {pkg_version}, run:\n  {unpin}brew upgrade {full_name}"
                    )),
                };
            }
            if only_dependencies {
                return Already::NotInstalled;
            }
            return Already::Installed {
                notice: Notice::Warning(format!(
                    "{full_name} {pkg_version} is already installed and up-to-date.\nTo reinstall {pkg_version}, run:\n  brew reinstall {name}"
                )),
            };
        }
    }

    let installed_this_version = kegs
        .iter()
        .any(|k| k.version == PkgVersion::parse(&pkg_version));
    let linked = linked_version(cfg, name);

    if installed_this_version {
        let message = format!("{full_name} {pkg_version} is already installed");
        return match &linked {
            Some(v) if *v != pkg_version => Already::Installed {
                notice: Notice::Warning(format!(
                    "{message}.\nThe currently linked version is: {v}"
                )),
            },
            None if only_dependencies => Already::NotInstalled,
            None => Already::Installed {
                notice: Notice::Warning(format!(
                    "{message}, it's just not linked.\nTo link this version, run:\n  brew link {full_name}"
                )),
            },
            Some(_) => Already::Installed {
                notice: Notice::Warning(format!(
                    "{message} and up-to-date.\nTo reinstall {pkg_version}, run:\n  brew reinstall {name}"
                )),
            },
        };
    }

    // Only other versions are installed.
    if outdated && !cfg.no_install_upgrade && !pinned {
        return Already::Outdated;
    }
    if only_dependencies {
        return Already::NotInstalled;
    }
    match linked {
        Some(installed) if outdated => Already::Installed {
            notice: Notice::Error(format!(
                "{name} {installed} is already installed\nTo upgrade to {pkg_version}, run:\n  {unpin}brew upgrade {full_name}"
            )),
        },
        Some(installed) => Already::Installed {
            notice: Notice::Error(format!(
                "{name} {installed} is already installed\nTo install {pkg_version}, first run:\n  brew unlink {name}"
            )),
        },
        // Nothing is linked, so `FormulaInstaller` handles it: install the new
        // version beside the old one.
        None => Already::NotInstalled,
    }
}

/// Build the dependency closure for `roots`, in install order, dropping
/// dependencies that are already satisfied.
///
/// Outdated dependencies are included so they get upgraded, which is what
/// Homebrew's `Dependency#satisfied?` does by comparing the installed version
/// with the one the bottle tab pins.
pub fn dependency_closure(
    cfg: &Config,
    index: &Index,
    roots: &[FormulaEntry],
    ignore_dependencies: bool,
) -> Vec<FormulaEntry> {
    if ignore_dependencies {
        return vec![];
    }
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<FormulaEntry> = Vec::new();
    for root in roots {
        for name in deps::recursive_dependency_names(index, &root.name, DepOptions::default()) {
            if seen.contains(&name) {
                continue;
            }
            seen.push(name.clone());
            let Some(entry) = index.formula(&name) else {
                continue;
            };
            if is_satisfied(cfg, &entry) {
                continue;
            }
            out.push(entry);
        }
    }
    out
}

/// Every name `<name>` can be reached by, for the "installed under another
/// name" checks.
pub fn possible_names(formula: &FormulaEntry) -> Vec<String> {
    let mut names = vec![formula.name.clone()];
    names.extend(formula.aliases.iter().cloned());
    names.extend(formula.oldnames.iter().cloned());
    names
}

/// `FormulaInstaller#auto_link_versioned_keg_only?`: a versioned keg-only
/// formula the user asked for is linked anyway when nothing related is
/// installed yet.
pub fn auto_link_versioned_keg_only(cfg: &Config, item: &Item) -> bool {
    if !item.installed_on_request {
        return false;
    }
    let Some((reason, _)) = item.formula.keg_only() else {
        return false;
    };
    if reason != crate::model::KegOnly::VersionedFormula {
        return false;
    }
    if !item.existing.is_empty() {
        return false;
    }
    // `link_overwrite_formulae`: the unversioned formula and its siblings.
    let unversioned = item
        .formula
        .name
        .split_once('@')
        .map(|(base, _)| base.to_string())
        .unwrap_or_else(|| item.formula.name.clone());
    let mut related: Vec<String> = item.formula.versioned_formulae.clone();
    related.push(unversioned);
    !related
        .iter()
        .any(|name| !keg::installed_kegs(cfg, name).is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(value: serde_json::Value) -> FormulaEntry {
        let mut e: FormulaEntry = serde_json::from_value(value).unwrap();
        e.name = "demo".into();
        e.tap = "homebrew/core".into();
        e
    }

    #[test]
    fn disabled_formulae_are_refused_with_homebrews_wording() {
        let f = entry(json!({
            "disable_args": {":date": "2026-01-01", ":because": ":unmaintained"},
            "stable_version": "1.0"
        }));
        let err = check_deprecate_disable(&f, false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "demo has been disabled because it is not maintained upstream! It was disabled on 2026-01-01."
        );
        // `--force` downgrades it to a warning.
        assert!(check_deprecate_disable(&f, true).unwrap().is_some());
    }

    #[test]
    fn deprecated_formulae_only_warn() {
        let f = entry(json!({
            "deprecate_args": {":because": "it moved", ":replacement_formula": "other"},
            "stable_version": "1.0"
        }));
        let message = check_deprecate_disable(&f, false).unwrap().unwrap();
        assert_eq!(
            message,
            "demo has been deprecated because it it moved!\nReplacement:\n  brew install --formula other\n"
        );
    }

    #[test]
    fn default_prefix_bottles_are_refused_outside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let f = entry(json!({
            "pour_bottle_args": {":only_if": ":default_prefix"},
            "stable_version": "1.0",
            "bottle_checksum": "ab"
        }));
        let reason = pour_bottle_unsatisfied_reason(&cfg, &f).unwrap();
        assert!(
            reason.starts_with("The bottle needs to be installed into "),
            "{reason}"
        );
        let err = require_bottle(&cfg, &f).unwrap_err();
        assert!(matches!(err, Error::NeedsDelegation { .. }), "{err}");
    }

    #[test]
    fn a_formula_without_a_bottle_delegates() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let f = entry(json!({"stable_version": "1.0"}));
        let err = require_bottle(&cfg, &f).unwrap_err();
        assert_eq!(err.to_string(), "demo: no bottle available!");
    }

    #[test]
    fn bottle_references_carry_the_rebuild_and_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let f = entry(json!({
            "stable_version": "1.8.2", "bottle_rebuild": 1,
            "bottle_checksum": "ab", "revision": 0
        }));
        let bottle = bottle_for(&cfg, &f).unwrap();
        assert_eq!(bottle.pkg_version, "1.8.2");
        assert_eq!(bottle.rebuild, 1);
        assert_eq!(bottle.root_url, cfg.bottle_domain);

        let all = entry(json!({
            "stable_version": "3.10.0", "bottle_tag": ":all", "bottle_checksum": "cd"
        }));
        assert!(bottle_for(&cfg, &all).unwrap().tag.is_all());
    }

    #[test]
    fn conflict_message_matches_homebrew() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let message = conflict_message(
            &cfg,
            "demo",
            &[("other".to_string(), Some("both install `x`".to_string()))],
        );
        assert!(
            message.starts_with(
                "Cannot install demo because conflicting formulae are installed.\n  other: because both install `x`\n\n"
            ),
            "{message}"
        );
        assert!(message.contains("Please `brew unlink other` before continuing."));
        assert!(message.contains(&format!(
            "Unlinking removes a formula's symlinks from {}. You can",
            cfg.prefix.display()
        )));
    }
}
