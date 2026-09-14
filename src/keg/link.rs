//! `link`, `unlink`, `optlink` and their records (`docs/COMPAT.md` 5).
//!
//! `link` must implement Homebrew's per-directory rules (etc/bin/sbin/
//! include/share/lib/Frameworks), conflict handling (`ConflictError` message
//! text, `--overwrite`, `link_overwrite_paths` globs, `resolve_any_conflicts`
//! for symlinked directories), `--dry-run` listing, rollback on failure, the
//! `var/homebrew/linked/<name>` record, and `install-info` for info files.
//! `unlink` removes every prefix symlink resolving into the keg, prunes empty
//! directories it emptied, and removes the record. `optlink` maintains
//! `opt/<name>`, `opt/<alias>` and `opt/<oldname>` links.

use crate::config::Config;
use crate::error::Result;
use crate::keg::Keg;

#[derive(Debug, Clone, Copy, Default)]
pub struct LinkOptions {
    pub overwrite: bool,
    pub dry_run: bool,
    pub verbose: bool,
}

/// Number of symlinks created.
pub fn link(
    _cfg: &Config,
    _keg: &Keg,
    _overwrite_globs: &[String],
    _opts: LinkOptions,
) -> Result<usize> {
    todo!("keg::link::link")
}

/// Number of symlinks removed.
pub fn unlink(_cfg: &Config, _keg: &Keg, _opts: LinkOptions) -> Result<usize> {
    todo!("keg::link::unlink")
}

pub fn optlink(_cfg: &Config, _keg: &Keg, _aliases: &[String], _oldnames: &[String]) -> Result<()> {
    todo!("keg::link::optlink")
}

/// Remove `opt/<name>`, `var/homebrew/linked/<name>`, alias and oldname records for a keg being uninstalled.
pub fn remove_records(
    _cfg: &Config,
    _keg: &Keg,
    _aliases: &[String],
    _oldnames: &[String],
) -> Result<()> {
    todo!("keg::link::remove_records")
}
