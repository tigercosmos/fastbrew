//! Cask downloads with `url_kwargs` (user agent, referer, cookies, headers),
//! sha256 verification (`:no_check` skips), and Homebrew's cache naming
//! (`$CACHE/downloads/<sha256(url)>--<basename>`, symlink `$CACHE/<token>--<version>.<ext>`).

use std::path::PathBuf;

use crate::config::Config;
use crate::error::Result;
use crate::model::CaskEntry;

pub fn download_cask(_cfg: &Config, _cask: &CaskEntry, _quiet: bool) -> Result<PathBuf> {
    todo!("cask::download::download_cask")
}
