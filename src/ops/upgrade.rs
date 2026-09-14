//! `upgrade` for formulae: install the new version, unlink the old, relink,
//! migrate pinned/linked state, then clean up old kegs unless
//! `HOMEBREW_NO_INSTALL_CLEANUP`. Prints `==> Upgrading <name>` and
//! `  <old> -> <new>` like Homebrew.

use crate::api::index::Index;
use crate::config::Config;
use crate::error::Result;

#[derive(Debug, Clone, Default)]
pub struct UpgradeOptions {
    pub dry_run: bool,
    pub force: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub greedy: bool,
}

/// Empty `names` means everything outdated.
pub fn upgrade_formulae(
    _cfg: &Config,
    _index: &Index,
    _names: &[String],
    _opts: &UpgradeOptions,
) -> Result<()> {
    todo!("ops::upgrade::upgrade_formulae")
}
