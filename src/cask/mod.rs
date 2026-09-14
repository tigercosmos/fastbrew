//! Cask engine (`docs/DESIGN.md` 8, `docs/COMPAT.md` 6).

pub mod artifacts;
pub mod config;
pub mod download;
pub mod install;
pub mod uninstall;
pub mod unpack;

use std::path::PathBuf;

use crate::config::Config;

/// Installed cask discovered in the Caskroom.
#[derive(Debug, Clone)]
pub struct InstalledCask {
    pub token: String,
    pub version: String,
    pub caskroom_path: PathBuf,
    /// `.metadata/<version>/<timestamp>` of the latest install.
    pub metadata_path: Option<PathBuf>,
}

pub fn installed_casks(_cfg: &Config) -> Vec<InstalledCask> {
    todo!("cask::installed_casks")
}

pub fn installed_cask(_cfg: &Config, _token: &str) -> Option<InstalledCask> {
    todo!("cask::installed_cask")
}
