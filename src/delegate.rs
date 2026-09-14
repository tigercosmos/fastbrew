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
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;
use crate::error::{Error, Result};

fn is_self(path: &Path) -> bool {
    let Ok(me) = std::env::current_exe() else {
        return false;
    };
    match (std::fs::canonicalize(path), std::fs::canonicalize(&me)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn usable(path: PathBuf) -> Option<PathBuf> {
    if !path.is_file() || is_self(&path) {
        return None;
    }
    // Must be executable.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).ok()?.permissions().mode();
    (mode & 0o111 != 0).then_some(path)
}

pub fn find_brew(cfg: &Config) -> Option<PathBuf> {
    if let Some(explicit) = cfg.brew_path.clone() {
        return usable(explicit);
    }
    for candidate in [cfg.prefix.join("bin/brew"), cfg.repository.join("bin/brew")] {
        if let Some(p) = usable(candidate) {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if let Some(p) = usable(dir.join("brew")) {
            return Some(p);
        }
    }
    None
}

/// Never returns on success (process image is replaced).
pub fn exec_brew(
    cfg: &Config,
    args: &[OsString],
    reason: &str,
    quiet: bool,
) -> Result<std::convert::Infallible> {
    if cfg.no_delegate {
        return Err(Error::user(format!(
            "FASTBREW_NO_DELEGATE is set, refusing to delegate to brew ({reason})."
        )));
    }
    let Some(brew) = find_brew(cfg) else {
        return Err(Error::user(format!(
            "This needs Homebrew's `brew` ({reason}) but none was found.\nInstall Homebrew from https://brew.sh and try again, or set FASTBREW_BREW to its path."
        )));
    };
    if !quiet {
        eprintln!("fastbrew: delegating to brew ({reason})");
    }
    let error = Command::new(&brew).args(args).exec();
    Err(Error::user(format!(
        "Failed to exec {}: {error}",
        brew.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn finds_brew_in_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::from_env().unwrap();
        cfg.prefix = dir.path().join("prefix");
        cfg.repository = cfg.prefix.clone();
        cfg.brew_path = None;
        std::fs::create_dir_all(cfg.prefix.join("bin")).unwrap();
        assert!(find_brew(&cfg).is_none() || !find_brew(&cfg).unwrap().starts_with(&cfg.prefix));

        let brew = cfg.prefix.join("bin/brew");
        std::fs::write(&brew, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&brew, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_brew(&cfg), Some(brew));
    }

    #[test]
    fn non_executable_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brew");
        std::fs::write(&path, "x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(usable(path), None);
    }
}
