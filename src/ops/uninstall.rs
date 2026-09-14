//! `uninstall`/`remove`/`rm` and `autoremove` for formulae.

use crate::api::index::Index;
use crate::config::Config;
use crate::error::Result;

#[derive(Debug, Clone, Default)]
pub struct UninstallOptions {
    pub force: bool,
    pub ignore_dependencies: bool,
    pub dry_run: bool,
}

pub fn uninstall_formulae(
    _cfg: &Config,
    _index: &Index,
    _names: &[String],
    _opts: &UninstallOptions,
) -> Result<()> {
    todo!("ops::uninstall::uninstall_formulae")
}

pub fn autoremove(_cfg: &Config, _index: &Index, _dry_run: bool) -> Result<()> {
    todo!("ops::uninstall::autoremove")
}
