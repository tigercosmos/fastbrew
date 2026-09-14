//! Third-party taps under `$LIBRARY/Taps/<user>/homebrew-<repo>`.
//!
//! - `Tap::parse("user/repo")`, `Tap::installed(cfg)`, `path()`, `remote()`.
//! - `tap(cfg, name, url)`: `git clone --origin=origin --template= --config core.fsmonitor=false <url> <path>`;
//!   default URL `https://github.com/<user>/homebrew-<repo>`. Refuse
//!   `homebrew/core` and `homebrew/cask` without `--force` with Homebrew's
//!   message ("Tapping homebrew/core is no longer typically necessary...").
//! - `untap`: refuse when formulae from the tap are installed unless `--force`.
//! - `update_all`: `git fetch` + fast-forward each tap concurrently, returning
//!   which changed.
//! - `formula_files`, `cask_files`: `Formula/**/*.rb`, `Casks/**/*.rb`
//!   (also `HomebrewFormula/`), and `formula_names()` mapped to file paths.

use std::path::PathBuf;

use crate::config::Config;
use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Tap {
    pub user: String,
    pub repo: String,
}

impl Tap {
    pub fn parse(_name: &str) -> Option<Tap> {
        todo!("tap::Tap::parse")
    }

    pub fn name(&self) -> String {
        format!("{}/{}", self.user.to_lowercase(), self.repo.to_lowercase())
    }

    pub fn path(&self, cfg: &Config) -> PathBuf {
        cfg.taps_dir()
            .join(self.user.to_lowercase())
            .join(format!("homebrew-{}", self.repo.to_lowercase()))
    }

    pub fn default_remote(&self) -> String {
        format!("https://github.com/{}/homebrew-{}", self.user, self.repo)
    }

    pub fn is_core(&self) -> bool {
        self.name() == "homebrew/core"
    }

    pub fn is_cask(&self) -> bool {
        self.name() == "homebrew/cask"
    }
}

pub fn installed_taps(_cfg: &Config) -> Vec<Tap> {
    todo!("tap::installed_taps")
}

pub fn tap(
    _cfg: &Config,
    _name: &str,
    _url: Option<&str>,
    _force: bool,
    _quiet: bool,
) -> Result<()> {
    todo!("tap::tap")
}

pub fn untap(_cfg: &Config, _name: &str, _force: bool) -> Result<()> {
    todo!("tap::untap")
}

/// Returns the taps whose HEAD moved.
pub fn update_all(_cfg: &Config, _quiet: bool) -> Result<Vec<Tap>> {
    todo!("tap::update_all")
}

pub fn formula_files(_cfg: &Config, _tap: &Tap) -> Vec<(String, PathBuf)> {
    todo!("tap::formula_files")
}

pub fn cask_files(_cfg: &Config, _tap: &Tap) -> Vec<(String, PathBuf)> {
    todo!("tap::cask_files")
}
