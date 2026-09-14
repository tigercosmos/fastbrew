//! `cleanup` (port of `Library/Homebrew/cleanup.rb`): remove kegs that are not
//! the latest installed version and not linked or pinned, remove cached
//! downloads older than `HOMEBREW_CLEANUP_MAX_AGE_DAYS` or not matching the
//! current version (`--prune=days`, `-s` scrub removes all downloads), prune
//! old logs, remove stale lock files, and print the freed space summary
//! `==> This operation has freed approximately 1.2MB of disk space.`

use crate::api::index::Index;
use crate::config::Config;
use crate::error::Result;

#[derive(Debug, Clone, Default)]
pub struct CleanupOptions {
    pub dry_run: bool,
    pub scrub: bool,
    pub prune_days: Option<u64>,
    pub prune_prefix: bool,
}

/// Empty `names` cleans everything.
pub fn cleanup(
    _cfg: &Config,
    _index: &Index,
    _names: &[String],
    _opts: &CleanupOptions,
) -> Result<()> {
    todo!("ops::cleanup::cleanup")
}

/// Remove older kegs and unreferenced downloads for the given formulae after an install/upgrade.
pub fn cleanup_after_install(_cfg: &Config, _index: &Index, _names: &[String]) -> Result<()> {
    todo!("ops::cleanup::cleanup_after_install")
}
