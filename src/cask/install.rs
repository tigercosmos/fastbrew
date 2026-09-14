//! `install --cask`, `reinstall --cask`, `upgrade --cask`.

use crate::api::index::Index;
use crate::config::Config;
use crate::error::Result;

#[derive(Debug, Clone, Default)]
pub struct CaskInstallOptions {
    pub force: bool,
    pub adopt: bool,
    pub skip_cask_deps: bool,
    pub dry_run: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub reinstall: bool,
    pub explicit_dir_flags: Vec<String>,
}

pub fn install_casks(
    _cfg: &Config,
    _index: &Index,
    _tokens: &[String],
    _opts: &CaskInstallOptions,
) -> Result<()> {
    todo!("cask::install::install_casks")
}

pub fn upgrade_casks(
    _cfg: &Config,
    _index: &Index,
    _tokens: &[String],
    _greedy: bool,
    _opts: &CaskInstallOptions,
) -> Result<()> {
    todo!("cask::install::upgrade_casks")
}

pub fn fetch_casks(_cfg: &Config, _index: &Index, _tokens: &[String], _force: bool) -> Result<()> {
    todo!("cask::install::fetch_casks")
}
