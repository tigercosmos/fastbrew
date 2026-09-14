//! Typed views of Homebrew's data: internal-API formula and cask entries,
//! install receipts, cask receipts and cask config.
//!
//! The internal API serializes Ruby symbols as strings with a leading colon
//! (`":build"`); helper functions here strip them. Optional keys are absent,
//! never null. See `docs/COMPAT.md` section 1.3.

pub mod cask;
pub mod formula;
pub mod receipt;

pub use cask::{CaskEntry, CaskName};
pub use formula::{Dependency, DependencyTag, FormulaEntry, KegOnly, UsesFromMacos};
pub use receipt::{CaskReceipt, FormulaReceipt, RuntimeDependency};

/// Strip a Ruby symbol's leading colon: `":build"` -> `"build"`.
pub fn sym(s: &str) -> &str {
    s.strip_prefix(':').unwrap_or(s)
}
