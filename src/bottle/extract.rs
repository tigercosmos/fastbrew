//! Extract a bottle tarball into the Cellar.
//!
//! The archive contains `<name>/<version>/...`. Extract into a temporary
//! directory inside the rack (`$CELLAR/<name>/.fastbrew-<random>`), then
//! rename `<tmp>/<name>/<version>` to `$CELLAR/<name>/<version>`. Preserve
//! modes, mtimes, symlinks and hard links. Fail if the keg already exists
//! unless `replace` (used by `reinstall`), in which case the old keg is
//! removed after a successful extraction.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::Result;

pub fn extract_bottle(
    _cfg: &Config,
    _tarball: &Path,
    _name: &str,
    _pkg_version: &str,
    _replace: bool,
) -> Result<PathBuf> {
    todo!("bottle::extract::extract_bottle")
}
