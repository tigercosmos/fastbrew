//! `install` and `reinstall` for formulae (bottles only; source builds delegate).

use crate::api::index::Index;
use crate::config::Config;
use crate::error::Result;

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub only_dependencies: bool,
    pub ignore_dependencies: bool,
    pub force: bool,
    pub dry_run: bool,
    pub overwrite: bool,
    pub skip_post_install: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub reinstall: bool,
    /// Mark as installed on request (false for dependencies).
    pub on_request: bool,
    pub build_from_source: bool,
    pub head: bool,
    pub keep_tmp: bool,
}

/// Plan and execute installation of `names` (already resolved formula names).
pub fn install_formulae(
    _cfg: &Config,
    _index: &Index,
    _names: &[String],
    _opts: &InstallOptions,
) -> Result<()> {
    todo!("ops::install::install_formulae")
}

/// `fetch`: download manifests and blobs only.
pub fn fetch_formulae(
    _cfg: &Config,
    _index: &Index,
    _names: &[String],
    _deps: bool,
    _force: bool,
) -> Result<()> {
    todo!("ops::install::fetch_formulae")
}
