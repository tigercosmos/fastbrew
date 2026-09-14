//! Delegation to the Ruby `brew` for unsupported paths.
//!
//! Lookup order: `FASTBREW_BREW`, `$HOMEBREW_PREFIX/bin/brew`,
//! `$HOMEBREW_REPOSITORY/bin/brew`, `brew` on `PATH` (skipping fastbrew
//! itself if it is installed under that name). Prints
//! `fastbrew: delegating to brew (<reason>)` to stderr unless quiet, then
//! `exec`s `brew <args>` with the environment untouched. When
//! `FASTBREW_NO_DELEGATE` is set or no brew exists, fails with a message
//! naming the reason and `https://brew.sh`.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::config::Config;
use crate::error::Result;

pub fn find_brew(_cfg: &Config) -> Option<PathBuf> {
    todo!("delegate::find_brew")
}

/// Never returns on success (process image is replaced).
pub fn exec_brew(
    _cfg: &Config,
    _args: &[OsString],
    _reason: &str,
    _quiet: bool,
) -> Result<std::convert::Infallible> {
    todo!("delegate::exec_brew")
}
