//! Cask artifact install/uninstall phases (port of `Library/Homebrew/cask/artifact/*`).
//!
//! Supported kinds are listed in `docs/DESIGN.md` 5. `uninstall_artifacts_json`
//! produces the v2-style list written into the cask receipt.

use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::model::CaskEntry;

use super::config::CaskDirs;

#[derive(Debug, Clone, Copy, Default)]
pub struct ArtifactOptions {
    pub force: bool,
    pub adopt: bool,
    pub verbose: bool,
    pub dry_run: bool,
    pub skip_binaries: bool,
}

pub fn install_artifacts(
    _cfg: &Config,
    _dirs: &CaskDirs,
    _cask: &CaskEntry,
    _staged: &Path,
    _opts: ArtifactOptions,
) -> Result<()> {
    todo!("cask::artifacts::install_artifacts")
}

pub fn uninstall_artifacts(
    _cfg: &Config,
    _dirs: &CaskDirs,
    _cask: &CaskEntry,
    _staged: &Path,
    _zap: bool,
    _opts: ArtifactOptions,
) -> Result<()> {
    todo!("cask::artifacts::uninstall_artifacts")
}

pub fn uninstall_artifacts_json(
    _cfg: &Config,
    _dirs: &CaskDirs,
    _cask: &CaskEntry,
    _staged: &Path,
) -> Vec<serde_json::Value> {
    todo!("cask::artifacts::uninstall_artifacts_json")
}
