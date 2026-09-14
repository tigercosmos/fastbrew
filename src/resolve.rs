//! Name resolution for formulae and casks.
//!
//! Order for a formula reference `ref` (port of `Formulary.loader_for` for the
//! API and tap cases). First, `user/repo/name` selects a third-party tap
//! formula (via `tap` + `rubylite`); `homebrew/core/name` and
//! `Homebrew/homebrew-core/name` mean core. Second, an exact core name.
//! Third, a core alias (`formula_aliases`), then a rename (`formula_renames`),
//! then a tap migration (resolve to the tap's formula if the tap is installed,
//! else error naming the tap). Fourth, an installed keg by name (for formulae
//! removed from the API): build a `FormulaEntry` from the receipt with only
//! the fields the receipt knows. Casks: token, then `cask_renames`, then tap
//! migration, then installed Caskroom entry.
//!
//! Errors are `Error::Unavailable` with up to three suggestions (Levenshtein
//! distance ≤ 2 or prefix matches, from the index).

use crate::api::index::Index;
use crate::config::Config;
use crate::error::Result;
use crate::model::{CaskEntry, FormulaEntry};

/// What a user-supplied name resolved to.
#[derive(Debug, Clone)]
pub enum Resolved {
    Formula(FormulaEntry),
    Cask(CaskEntry),
}

/// Restrict resolution to one kind (`--formula` / `--cask`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Any,
    Formula,
    Cask,
}

pub fn resolve(_cfg: &Config, _index: &Index, _name: &str, _kind: Kind) -> Result<Resolved> {
    todo!("resolve::resolve")
}

pub fn resolve_formula(_cfg: &Config, _index: &Index, _name: &str) -> Result<FormulaEntry> {
    todo!("resolve::resolve_formula")
}

pub fn resolve_cask(_cfg: &Config, _index: &Index, _token: &str) -> Result<CaskEntry> {
    todo!("resolve::resolve_cask")
}

/// Suggestions for an unknown name, for error messages.
pub fn suggestions(_index: &Index, _name: &str, _kind: Kind) -> Vec<String> {
    todo!("resolve::suggestions")
}
