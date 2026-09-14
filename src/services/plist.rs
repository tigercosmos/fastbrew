//! Generate `homebrew.mxcl.<name>.plist` and `homebrew.<name>.service` from
//! the API `service_args`/`service_run_args` (port of `service.rb#to_plist`
//! and `#to_systemd_unit`). Placeholders `$HOMEBREW_PREFIX`, `$HOMEBREW_CELLAR`
//! and `/$HOME` are replaced first.

use crate::config::Config;
use crate::error::Result;
use crate::model::FormulaEntry;

/// launchd label, default `homebrew.mxcl.<name>`.
pub fn plist_label(_formula: &FormulaEntry) -> String {
    todo!("services::plist::plist_label")
}

/// XML plist text, or `None` when the formula has no runnable service.
pub fn to_plist(_cfg: &Config, _formula: &FormulaEntry) -> Result<Option<String>> {
    todo!("services::plist::to_plist")
}

pub fn to_systemd_unit(_cfg: &Config, _formula: &FormulaEntry) -> Result<Option<String>> {
    todo!("services::plist::to_systemd_unit")
}

/// Write both files into the keg (called at install time).
pub fn install_service_files(
    _cfg: &Config,
    _formula: &FormulaEntry,
    _keg_path: &std::path::Path,
) -> Result<()> {
    todo!("services::plist::install_service_files")
}
