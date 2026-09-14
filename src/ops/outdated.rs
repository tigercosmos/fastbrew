//! Outdated detection for formulae and casks (`docs/COMPAT.md` 8).

use crate::api::index::Index;
use crate::config::Config;
use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutdatedFormula {
    pub name: String,
    pub installed_versions: Vec<String>,
    pub current_version: String,
    pub pinned: bool,
    pub pinned_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutdatedCask {
    pub token: String,
    pub installed_version: String,
    pub current_version: String,
}

pub fn outdated_formulae(
    _cfg: &Config,
    _index: &Index,
    _names: Option<&[String]>,
) -> Result<Vec<OutdatedFormula>> {
    todo!("ops::outdated::outdated_formulae")
}

pub fn outdated_casks(
    _cfg: &Config,
    _index: &Index,
    _names: Option<&[String]>,
    _greedy: bool,
) -> Result<Vec<OutdatedCask>> {
    todo!("ops::outdated::outdated_casks")
}
