//! Name resolution for formulae and casks.
//!
//! Order for a formula reference `ref` (port of `Formulary.loader_for` for the
//! API, tap and keg cases). First, `user/repo/name` selects that tap's
//! formula (`FromTapLoader`); `homebrew/core/name` and
//! `Homebrew/homebrew-core/name` mean core. Second, `FromAPILoader`: an exact
//! core name, a core alias (`formula_aliases`), a rename
//! (`formula_renames`) or an oldname. Third, `FromNameLoader`: a bare name
//! that exists only in installed third-party taps — the migration target tap
//! first, then every other installed tap; more than one match is
//! `TapFormulaAmbiguityError`. Fourth, a tap migration whose tap is not
//! installed, reported with Homebrew's `brew tap` hint. Fifth,
//! `FromKegLoader`: an installed keg by name (for formulae removed from the
//! API), with a `FormulaEntry` built from the receipt.
//!
//! Casks follow the same shape: `user/repo/token`, then the API token and
//! `cask_renames`, then installed taps, then a tap migration, then an
//! installed Caskroom entry.
//!
//! Errors are `Error::Unavailable` with the suggestions Homebrew's
//! `DidYouMean::SpellChecker` would produce.

use crate::api::index::Index;
use crate::api::taps::TapIndex;
use crate::config::Config;
use crate::error::{Error, PackageKind, Result};
use crate::model::{CaskEntry, FormulaEntry};
use crate::platform::Host;
use crate::tap::Tap;
use crate::version::PkgVersion;

/// What a user-supplied name resolved to.
#[derive(Debug, Clone)]
pub enum Resolved {
    Formula(FormulaEntry),
    Cask(CaskEntry),
}

impl Resolved {
    pub fn name(&self) -> String {
        match self {
            Resolved::Formula(f) => f.name.clone(),
            Resolved::Cask(c) => c.token.clone(),
        }
    }
}

/// Restrict resolution to one kind (`--formula` / `--cask`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Any,
    Formula,
    Cask,
}

pub fn resolve(cfg: &Config, index: &Index, name: &str, kind: Kind) -> Result<Resolved> {
    match kind {
        Kind::Formula => resolve_formula(cfg, index, name).map(Resolved::Formula),
        Kind::Cask => resolve_cask(cfg, index, name).map(Resolved::Cask),
        Kind::Any => {
            if let Ok(f) = resolve_formula(cfg, index, name) {
                warn_if_cask_conflicts(cfg, index, name);
                return Ok(Resolved::Formula(f));
            }
            match resolve_cask(cfg, index, name) {
                Ok(c) => Ok(Resolved::Cask(c)),
                // Report the formula error: that is what Homebrew shows for an
                // unqualified name that is neither.
                Err(_) => resolve_formula(cfg, index, name).map(Resolved::Formula),
            }
        }
    }
}

/// `NamedArgs#warn_if_cask_conflicts`: a name that is both a formula and a
/// cask resolves to the formula, with a warning naming the cask.
///
/// Only an exact token counts: `return if cask.old_tokens.include?(ref)`
/// means a name that reaches a cask through a rename is not a conflict. That
/// also keeps this off the slow path, since it never needs a cask's metadata.
fn warn_if_cask_conflicts(cfg: &Config, index: &Index, reference: &str) {
    if reference.contains('/') || crate::output::is_quiet() {
        return;
    }
    let tap = if index.has_cask(reference) {
        "homebrew/cask".to_string()
    } else {
        let found = crate::tap::installed_taps(cfg)
            .into_iter()
            .filter(|t| !t.is_core() && !t.is_cask())
            .find(|t| {
                crate::tap::cask_files(cfg, t)
                    .iter()
                    .any(|(token, _)| token == reference)
            });
        match found {
            Some(t) => t.name(),
            None => return,
        }
    };
    // `package_conflicts_message`: the fully-qualified token is only offered
    // when the cask has a tap, which every API and tap cask does.
    crate::output::opoo(&format!(
        "Treating {reference} as a formula. For the cask, use {tap}/{reference} or specify the \
         `--cask` flag. To silence this message, use the `--formula` flag."
    ));
}

/// Split `user/repo/name` into its tap and the bare name.
fn split_tap_ref(reference: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = reference.split('/').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let user = parts[0].to_lowercase();
    let repo = parts[1]
        .to_lowercase()
        .trim_start_matches("homebrew-")
        .to_string();
    Some((format!("{user}/{repo}"), parts[2].to_string()))
}

/// Load the metadata of every installed third-party tap.
///
/// Only the paths that miss in the API call this, so the common lookup never
/// touches a tap.
fn tap_index(cfg: &Config) -> TapIndex {
    TapIndex::load(cfg, &Host::detect().bottle_tag())
}

/// `TapFormulaUnavailableError#to_s` for a tap that is not installed.
fn needs_tap(reference: &str, tap: &str) -> Error {
    Error::user(format!(
        "No available formula or cask with the name \"{reference}\".\nThis command requires the tap {tap}.\nIf you trust this tap, tap it explicitly and then try again:\n  brew tap {tap}"
    ))
}

/// `TapFormulaAmbiguityError`: a bare name carried by several installed taps.
fn ambiguous(name: &str, taps: &[String]) -> Error {
    let list: String = taps
        .iter()
        .map(|t| format!("\n       * {t}/{name}"))
        .collect();
    Error::user(format!(
        "Formulae found in multiple taps:{list}\n\nPlease use the fully-qualified name (e.g. {}/{name}) to refer to a specific formula.",
        taps.first().map(String::as_str).unwrap_or_default()
    ))
}

pub fn resolve_formula(cfg: &Config, index: &Index, reference: &str) -> Result<FormulaEntry> {
    let mut name = reference.to_string();
    if let Some((tap, bare)) = split_tap_ref(reference) {
        if tap != "homebrew/core" {
            return tap_formula(cfg, reference, &tap, &bare);
        }
        name = bare;
    }

    if let Some(entry) = index.formula(&name) {
        return Ok(entry);
    }
    if let Some(target) = index.formula_alias(&name)
        && let Some(entry) = index.formula(&target)
    {
        return Ok(entry);
    }
    if let Some(target) = index.formula_rename(&name)
        && let Some(entry) = index.formula(&target)
    {
        return Ok(entry);
    }
    if let Some(target) = index.formula_oldname(&name)
        && let Some(entry) = index.formula(&target)
    {
        return Ok(entry);
    }

    // `homebrew/cask/google-cloud-sdk` style targets name the new token.
    let migration =
        index.formula_tap_migration(&name).map(
            |tap| match tap.split('/').collect::<Vec<_>>()[..] {
                [user, repo, rest] => (format!("{user}/{repo}"), rest.to_string()),
                _ => (tap.clone(), name.clone()),
            },
        );

    // `FromNameLoader`: the migration target first, then every installed tap.
    let taps = tap_index(cfg);
    if let Some((tap_name, new_name)) = &migration
        && let Some(meta) = taps.get(tap_name)
        && let Some(found) = meta.formula(new_name)
    {
        return found.map(|f| f.entry).map_err(needs_delegation);
    }
    let hits = taps.taps_with_formula(&name);
    match hits.len() {
        0 => {}
        1 => {
            return hits[0]
                .formula(&name)
                .expect("tap reported the name")
                .map(|f| f.entry)
                .map_err(needs_delegation);
        }
        _ => {
            let names: Vec<String> = hits.iter().map(|m| m.tap.name()).collect();
            return Err(ambiguous(&name, &names));
        }
    }

    if let Some((tap_name, new_name)) = migration {
        if tap_name == "homebrew/cask"
            && let Some(cask) = index.cask(&new_name)
        {
            return Err(Error::user(format!(
                "Formula {name} was migrated to the {tap_name} tap.\nInstall it with:\n  fastbrew install --cask {}",
                cask.token
            )));
        }
        return Err(Error::user(format!(
            "Formula {name} was migrated to the {tap_name} tap.\nIf you trust this tap, tap it explicitly and then try again:\n  brew tap {tap_name}"
        )));
    }
    if let Some(entry) = formula_from_installed_keg(cfg, &name) {
        return Ok(entry);
    }

    Err(Error::Unavailable {
        name: reference.to_string(),
        kind: PackageKind::Formula,
        suggestions: suggestions(index, &name, Kind::Formula),
        dependent: None,
    })
}

/// `Formulary.from_rack`: an installed formula resolves against the tap its
/// receipt records, so a keg poured from `user/repo` is never mistaken for the
/// core formula of the same name. `list`, `outdated` and `upgrade` walk racks,
/// whose directory names are bare.
pub fn resolve_installed(cfg: &Config, index: &Index, name: &str) -> Result<FormulaEntry> {
    if let Some(tap) = installed_tap(cfg, name)
        && let Ok(entry) = resolve_formula(cfg, index, &format!("{tap}/{name}"))
    {
        return Ok(entry);
    }
    resolve_formula(cfg, index, name)
}

/// `Tab.for_keg(rack).tap` when it names a third-party tap.
pub fn installed_tap(cfg: &Config, name: &str) -> Option<String> {
    let tap = crate::keg::latest_keg(cfg, name)?
        .receipt()
        .ok()?
        .tap()?
        .to_string();
    (tap.contains('/') && tap != "homebrew/core").then_some(tap)
}

/// `Dependency#to_formula` for a `depends_on` reached from `dependent`.
///
/// `user/repo/name` selects that tap. A bare name takes `Formulary`'s order:
/// the core API first, then — because a tap's formulae are loadable from
/// within the tap — the dependent's own tap, then `FromNameLoader` over every
/// other installed tap. A required dependency that resolves nowhere raises
/// `FormulaUnavailableError` naming the dependent; it is never skipped.
pub fn resolve_dependency(
    cfg: &Config,
    index: &Index,
    name: &str,
    dependent: &FormulaEntry,
) -> Result<FormulaEntry> {
    if name.contains('/') {
        return resolve_formula(cfg, index, name).map_err(|e| with_dependent(e, dependent));
    }
    if let Some(entry) = index.formula(name) {
        return Ok(entry);
    }
    if !is_core_tap(&dependent.tap)
        && let Some(tap) = Tap::parse(&dependent.tap)
        && tap.is_installed(cfg)
    {
        let meta = crate::api::taps::load_tap(cfg, &tap, &Host::detect().bottle_tag());
        if let Some(found) = meta.formula(name) {
            return found.map(|f| f.entry).map_err(needs_delegation);
        }
    }
    resolve_formula(cfg, index, name).map_err(|e| with_dependent(e, dependent))
}

/// `homebrew/core` (or an entry with no tap at all) is the API, not a tap.
pub fn is_core_tap(tap: &str) -> bool {
    tap.is_empty() || tap == "homebrew/core"
}

/// Attach `FormulaUnavailableError#dependent` to a failed dependency lookup.
fn with_dependent(error: Error, dependent: &FormulaEntry) -> Error {
    match error {
        Error::Unavailable {
            name,
            kind,
            suggestions,
            ..
        } => Error::Unavailable {
            name,
            kind,
            suggestions,
            dependent: Some(dependent.full_name()),
        },
        other => other,
    }
}

/// A formula `rubylite` could not extract has to go to the Ruby `brew`.
fn needs_delegation(reason: String) -> Error {
    Error::NeedsDelegation {
        reason: format!("fastbrew cannot read this tap formula ({reason})"),
    }
}

/// `FromTapLoader`: `user/repo/name` with the tap installed.
fn tap_formula(cfg: &Config, reference: &str, tap_name: &str, name: &str) -> Result<FormulaEntry> {
    let Some(tap) = Tap::parse(tap_name) else {
        return Err(Error::user(format!("Invalid tap name: '{tap_name}'")));
    };
    if !tap.is_installed(cfg) {
        return Err(needs_tap(reference, &tap.name()));
    }
    let meta = crate::api::taps::load_tap(cfg, &tap, &Host::detect().bottle_tag());
    match meta.formula(name) {
        Some(found) => found.map(|f| f.entry).map_err(needs_delegation),
        None => Err(Error::Unavailable {
            name: reference.to_string(),
            kind: PackageKind::Formula,
            suggestions: spell_check(name, &meta.formula_names()),
            dependent: None,
        }),
    }
}

pub fn resolve_cask(cfg: &Config, index: &Index, reference: &str) -> Result<CaskEntry> {
    let mut token = reference.to_string();
    if let Some((tap, bare)) = split_tap_ref(reference) {
        if tap != "homebrew/cask" {
            return tap_cask(cfg, reference, &tap, &bare);
        }
        token = bare;
    }

    if let Some(entry) = index.cask(&token) {
        return Ok(entry);
    }
    if let Some(target) = index.cask_rename(&token)
        && let Some(entry) = index.cask(&target)
    {
        return Ok(entry);
    }

    let taps = tap_index(cfg);
    let hits = taps.taps_with_cask(&token);
    if let Some(meta) = hits.first() {
        return meta
            .cask(&token)
            .expect("tap reported the token")
            .map_err(needs_delegation_cask);
    }

    if let Some(tap) = index.cask_tap_migration(&token) {
        return Err(Error::user(format!(
            "Cask {token} was migrated to the {tap} tap."
        )));
    }
    if let Some(entry) = cask_from_caskroom(cfg, &token) {
        return Ok(entry);
    }

    // Homebrew's wording for an unknown cask (`Cask::CaskUnavailableError`).
    Err(Error::user(format!(
        "Cask '{reference}' is unavailable: No Cask with this name exists."
    )))
}

fn needs_delegation_cask(reason: String) -> Error {
    Error::NeedsDelegation {
        reason: format!("fastbrew cannot read this tap cask ({reason})"),
    }
}

/// `Cask::CaskLoader::FromTapPathLoader`: `user/repo/token`.
fn tap_cask(cfg: &Config, reference: &str, tap_name: &str, token: &str) -> Result<CaskEntry> {
    let Some(tap) = Tap::parse(tap_name) else {
        return Err(Error::user(format!("Invalid tap name: '{tap_name}'")));
    };
    if !tap.is_installed(cfg) {
        return Err(needs_tap(reference, &tap.name()));
    }
    let meta = crate::api::taps::load_tap(cfg, &tap, &Host::detect().bottle_tag());
    match meta.cask(token) {
        Some(found) => found.map_err(needs_delegation_cask),
        None => Err(Error::user(format!(
            "Cask '{reference}' is unavailable: No Cask with this name exists."
        ))),
    }
}

/// Build a minimal entry for a formula that is installed but no longer in the
/// API, using only what the receipt and the keg directory name know.
fn formula_from_installed_keg(cfg: &Config, name: &str) -> Option<FormulaEntry> {
    let keg = crate::keg::latest_keg(cfg, name)?;
    let receipt = keg.receipt().ok();
    let pkg_version = PkgVersion::parse(&keg.version.to_string());
    let mut entry = FormulaEntry {
        name: name.to_string(),
        tap: receipt
            .as_ref()
            .and_then(|r| r.tap().map(str::to_string))
            .unwrap_or_else(|| "homebrew/core".to_string()),
        stable_version: Some(pkg_version.version.to_string()),
        revision: pkg_version.revision,
        ..Default::default()
    };
    if let Some(r) = &receipt
        && let Some(versions) = r.source.versions.as_ref()
    {
        entry.version_scheme = versions.version_scheme;
    }
    Some(entry)
}

/// Build a minimal cask entry from an installed Caskroom directory.
fn cask_from_caskroom(cfg: &Config, token: &str) -> Option<CaskEntry> {
    let dir = cfg.caskroom().join(token);
    if !dir.is_dir() {
        return None;
    }
    let version = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .find(|n| !n.starts_with('.'));
    Some(CaskEntry {
        token: token.to_string(),
        version,
        ..Default::default()
    })
}

/// Suggestions for an unknown name, for error messages.
///
/// Port of `DidYouMean::SpellChecker#correct` over `Formula.names`, which is
/// what `FormulaUnavailableError#did_you_mean` uses.
pub fn suggestions(index: &Index, name: &str, kind: Kind) -> Vec<String> {
    if kind == Kind::Cask {
        return spell_check(name, &index.cask_tokens());
    }
    spell_check(name, &index.formula_names())
}

/// `Utils::Text.to_sentence(..., conjunction: "or")`: `a`, `a or b`, `a, b or c`.
pub fn to_sentence(items: &[String], conjunction: &str) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} {conjunction} {b}"),
        _ => {
            let (last, rest) = items.split_last().unwrap();
            format!("{} {conjunction} {last}", rest.join(", "))
        }
    }
}

// ---------------------------------------------------------------------------
// DidYouMean port
// ---------------------------------------------------------------------------

fn normalize(s: &str) -> Vec<char> {
    s.to_lowercase().chars().filter(|c| *c != '@').collect()
}

/// Port of `DidYouMean::SpellChecker#correct`.
pub fn spell_check(input: &str, dictionary: &[String]) -> Vec<String> {
    let normalized_input = normalize(input);
    let threshold = if normalized_input.len() > 3 {
        0.834
    } else {
        0.77
    };

    let mut words: Vec<(&String, f64)> = dictionary
        .iter()
        .filter(|w| jaro_winkler(&normalize(w), &normalized_input) >= threshold)
        .filter(|w| w.as_str() != input)
        .map(|w| {
            let raw: Vec<char> = w.chars().collect();
            (w, jaro_winkler(&raw, &normalized_input))
        })
        .collect();
    // `sort_by!` ascending then `reverse!`.
    words.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    words.reverse();

    let lev_threshold = (normalized_input.len() as f64 * 0.25).ceil() as usize;
    let mistypes: Vec<String> = words
        .iter()
        .filter(|(w, _)| levenshtein(&normalize(w), &normalized_input) <= lev_threshold)
        .map(|(w, _)| (*w).clone())
        .collect();
    if !mistypes.is_empty() {
        return mistypes;
    }
    words
        .iter()
        .filter(|(w, _)| {
            let word = normalize(w);
            let length = normalized_input.len().min(word.len());
            levenshtein(&word, &normalized_input) < length
        })
        .map(|(w, _)| (*w).clone())
        .take(1)
        .collect()
}

/// Port of `DidYouMean::Jaro.distance`.
fn jaro(s1: &[char], s2: &[char]) -> f64 {
    let (a, b) = if s1.len() > s2.len() {
        (s2, s1)
    } else {
        (s1, s2)
    };
    let (l1, l2) = (a.len(), b.len());
    if l1 == 0 || l2 == 0 {
        return 0.0;
    }
    let range = if l2 > 3 { l2 / 2 - 1 } else { 0 };
    let mut flags1 = vec![false; l1];
    let mut flags2 = vec![false; l2];
    let mut m = 0.0f64;
    for i in 0..l1 {
        let last = i + range;
        let mut j = i.saturating_sub(range);
        while j <= last && j < l2 {
            if !flags2[j] && a[i] == b[j] {
                flags2[j] = true;
                flags1[i] = true;
                m += 1.0;
                break;
            }
            j += 1;
        }
    }
    if m == 0.0 {
        return 0.0;
    }
    let mut t = 0.0f64;
    let mut k = 0usize;
    for i in 0..l1 {
        if !flags1[i] {
            continue;
        }
        let mut j = k;
        let mut index = k;
        let mut next_k = l2;
        while j < l2 {
            index = j;
            if flags2[j] {
                next_k = j + 1;
                break;
            }
            j += 1;
        }
        k = next_k;
        if b.get(index) != Some(&a[i]) {
            t += 1.0;
        }
    }
    let t = (t / 2.0).floor();
    (m / l1 as f64 + m / l2 as f64 + (m - t) / m) / 3.0
}

/// Port of `DidYouMean::JaroWinkler.distance` (weight 0.1, threshold 0.7).
fn jaro_winkler(s1: &[char], s2: &[char]) -> f64 {
    let d = jaro(s1, s2);
    if d <= 0.7 {
        return d;
    }
    let mut prefix = 0usize;
    for c in s1 {
        if prefix < 4 && s2.get(prefix) == Some(c) {
            prefix += 1;
        } else {
            break;
        }
    }
    d + (prefix as f64 * 0.1 * (1.0 - d))
}

/// Standard Levenshtein distance over character slices.
fn levenshtein(a: &[char], b: &[char]) -> usize {
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn levenshtein_basics() {
        let a: Vec<char> = "kitten".chars().collect();
        let b: Vec<char> = "sitting".chars().collect();
        assert_eq!(levenshtein(&a, &b), 3);
        assert_eq!(levenshtein(&[], &b), 7);
    }

    #[test]
    fn jaro_winkler_matches_ruby() {
        let a: Vec<char> = "martha".chars().collect();
        let b: Vec<char> = "marhta".chars().collect();
        assert!((jaro(&a, &b) - 0.944_444).abs() < 1e-5);
        assert!((jaro_winkler(&a, &b) - 0.961_111).abs() < 1e-5);
    }

    #[test]
    fn spell_check_finds_near_names() {
        let d = dict(&["jq", "jqp", "jql", "jaq", "wget", "wget2", "cmake"]);
        let got = spell_check("jqq", &d);
        assert!(got.contains(&"jq".to_string()), "got {got:?}");
        assert!(got.contains(&"jqp".to_string()), "got {got:?}");
        assert!(!got.contains(&"cmake".to_string()));

        let got = spell_check("wgett", &d);
        assert_eq!(got, vec!["wget".to_string(), "wget2".to_string()]);

        assert!(spell_check("zzzzzzzzzz", &d).is_empty());
    }

    #[test]
    fn sentences() {
        assert_eq!(to_sentence(&[], "or"), "");
        assert_eq!(to_sentence(&dict(&["a"]), "or"), "a");
        assert_eq!(to_sentence(&dict(&["a", "b"]), "or"), "a or b");
        assert_eq!(to_sentence(&dict(&["a", "b", "c"]), "or"), "a, b or c");
    }

    #[test]
    fn tap_refs() {
        assert_eq!(
            split_tap_ref("user/repo/name"),
            Some(("user/repo".to_string(), "name".to_string()))
        );
        assert_eq!(
            split_tap_ref("Homebrew/homebrew-core/jq"),
            Some(("homebrew/core".to_string(), "jq".to_string()))
        );
        assert_eq!(split_tap_ref("jq"), None);
        assert_eq!(split_tap_ref("a/b"), None);
    }
}
