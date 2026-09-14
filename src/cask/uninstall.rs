//! `uninstall --cask [--zap]`.

use crate::api::index::Index;
use crate::config::Config;
use crate::error::Result;

pub fn uninstall_casks(
    _cfg: &Config,
    _index: &Index,
    _tokens: &[String],
    _zap: bool,
    _force: bool,
) -> Result<()> {
    todo!("cask::uninstall::uninstall_casks")
}
