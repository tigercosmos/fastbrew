//! Container extraction into the staged Caskroom directory: dmg (`hdiutil`),
//! zip (`ditto -x -k`), tar/tgz/tbz/txz (native), pkg (copied as-is), naked
//! binaries (copied), nested containers (`container_args[":nested"]`).

use std::path::Path;

use crate::config::Config;
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Dmg,
    Zip,
    Tar,
    Pkg,
    Naked,
}

pub fn detect_container(_download: &Path, _type_override: Option<&str>) -> Container {
    todo!("cask::unpack::detect_container")
}

pub fn unpack(
    _cfg: &Config,
    _download: &Path,
    _container: Container,
    _dest: &Path,
    _verbose: bool,
) -> Result<()> {
    todo!("cask::unpack::unpack")
}
