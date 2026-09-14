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

use std::collections::BTreeSet;

use crate::api::index::Index;
use crate::config::Config;
use crate::error::Result;
use crate::model::{Dependency, FormulaEntry};

#[derive(Debug, Clone, Copy, Default)]
pub struct DepOptions {
    pub include_build: bool,
    pub include_test: bool,
    pub include_optional: bool,
    pub skip_recommended: bool,
    pub include_implicit: bool,
}

/// Direct dependencies of `formula` after applying `opts` and the
/// `uses_from_macos` platform rule.
pub fn direct_dependencies(
    _cfg: &Config,
    _formula: &FormulaEntry,
    _opts: DepOptions,
) -> Vec<Dependency> {
    todo!("deps::direct_dependencies")
}

/// Full transitive closure in install order (dependencies first).
pub fn recursive_dependencies(
    _cfg: &Config,
    _index: &Index,
    _formula: &FormulaEntry,
    _opts: DepOptions,
) -> Result<Vec<FormulaEntry>> {
    todo!("deps::recursive_dependencies")
}

/// Formulae (from the index or installed set) that depend on `name`.
pub fn uses(
    _cfg: &Config,
    _index: &Index,
    _name: &str,
    _recursive: bool,
    _installed_only: bool,
    _opts: DepOptions,
) -> Result<Vec<String>> {
    todo!("deps::uses")
}

/// Installed formulae nothing else installed depends on.
pub fn leaves(_cfg: &Config) -> Result<Vec<String>> {
    todo!("deps::leaves")
}

/// Formulae `autoremove` would uninstall.
pub fn removable(_cfg: &Config, _index: &Index) -> Result<BTreeSet<String>> {
    todo!("deps::removable")
}
