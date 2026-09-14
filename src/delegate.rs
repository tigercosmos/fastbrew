//! Delegation to the Ruby `brew` for unsupported paths.
//!
//! Lookup order: `FASTBREW_BREW`, `$HOMEBREW_PREFIX/bin/brew`,
//! `$HOMEBREW_REPOSITORY/bin/brew`, `brew` on `PATH` (skipping fastbrew
//! itself if it is installed under that name). Prints
//! `fastbrew: delegating to brew (<reason>)` to stderr unless quiet, then
//! `exec`s `brew <args>` with the environment untouched. When
//! `FASTBREW_NO_DELEGATE` is set or no brew exists, fails with a message
//! naming the reason and `https://brew.sh`.
//!
//! Sandbox guard: with `FASTBREW_REQUIRE_SANDBOX=1` the `PATH` fallback is
//! skipped entirely and any candidate resolving inside a standard Homebrew
//! prefix (`/opt/homebrew`, `/usr/local`, `/home/linuxbrew/.linuxbrew`) is
//! refused, so a sandboxed run can never drive the host Homebrew.

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

/// A candidate resolving into one of these is the host's Homebrew, which a
/// sandboxed run must never touch (`CLAUDE.md` rule 1).
fn in_standard_prefix(path: &Path) -> bool {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    [
        crate::config::DEFAULT_PREFIX_MACOS_ARM64,
        crate::config::DEFAULT_PREFIX_MACOS_X86_64,
        crate::config::DEFAULT_PREFIX_LINUX,
    ]
    .iter()
    .any(|p| resolved.starts_with(p))
}

pub fn find_brew(cfg: &Config) -> Option<PathBuf> {
    let allowed = |p: PathBuf| -> Option<PathBuf> {
        if cfg.require_sandbox && in_standard_prefix(&p) {
            return None;
        }
        Some(p)
    };
    if let Some(explicit) = cfg.brew_path.clone() {
        return usable(explicit).and_then(allowed);
    }
    for candidate in [cfg.prefix.join("bin/brew"), cfg.repository.join("bin/brew")] {
        if let Some(p) = usable(candidate).and_then(allowed) {
            return Some(p);
        }
    }
    // Inside a sandbox `PATH` still holds the host's `brew`, so it is not a
    // candidate at all; only an explicit or in-prefix `brew` may be used.
    if cfg.require_sandbox {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if let Some(p) = usable(dir.join("brew")) {
            return Some(p);
        }
    }
    None
}

/// Run the Ruby `brew` as a child process and wait for it.
///
/// `exec_brew` replaces this process, which is right for a command fastbrew
/// does not implement at all; a step *inside* a command (a Ruby
/// `post_install` during an install) has to come back here afterwards.
pub fn run_brew(
    cfg: &Config,
    args: &[OsString],
    reason: &str,
    quiet: bool,
) -> Result<std::process::ExitStatus> {
    let brew = brew_for(cfg, reason)?;
    if !quiet {
        eprintln!("fastbrew: delegating to brew ({reason})");
    }
    Command::new(&brew)
        .args(args)
        .status()
        .map_err(|e| Error::user(format!("Failed to run {}: {e}", brew.display())))
}

/// The `brew` a delegation may use, or the error explaining why there is none.
fn brew_for(cfg: &Config, reason: &str) -> Result<PathBuf> {
    if cfg.no_delegate {
        return Err(Error::user(format!(
            "FASTBREW_NO_DELEGATE is set, refusing to delegate to brew ({reason})."
        )));
    }
    find_brew(cfg).ok_or_else(|| {
        Error::user(format!(
            "This needs Homebrew's `brew` ({reason}) but none was found.\nInstall Homebrew from https://brew.sh and try again, or set FASTBREW_BREW to its path."
        ))
    })
}

/// Never returns on success (process image is replaced).
pub fn exec_brew(
    cfg: &Config,
    args: &[OsString],
    reason: &str,
    quiet: bool,
) -> Result<std::convert::Infallible> {
    let brew = brew_for(cfg, reason)?;
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

    fn executable(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn finds_brew_in_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_test(dir.path());
        cfg.repository = cfg.prefix.clone();
        std::fs::create_dir_all(cfg.prefix.join("bin")).unwrap();
        assert!(find_brew(&cfg).is_none() || !find_brew(&cfg).unwrap().starts_with(&cfg.prefix));

        let brew = cfg.prefix.join("bin/brew");
        executable(&brew);
        assert_eq!(find_brew(&cfg), Some(brew));
    }

    #[test]
    fn the_sandbox_guard_refuses_the_host_brew() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_test(dir.path());
        cfg.repository = cfg.prefix.clone();
        let brew = cfg.prefix.join("bin/brew");
        executable(&brew);

        // A `brew` inside the sandbox prefix stays usable.
        cfg.require_sandbox = true;
        assert_eq!(find_brew(&cfg), Some(brew.clone()));

        // The host's `brew` is refused however it is reached.
        let host = Path::new("/opt/homebrew/bin/brew");
        assert!(in_standard_prefix(host));
        assert!(in_standard_prefix(Path::new("/usr/local/bin/brew")));
        assert!(in_standard_prefix(Path::new(
            "/home/linuxbrew/.linuxbrew/bin/brew"
        )));
        assert!(!in_standard_prefix(&brew));
        if host.is_file() {
            cfg.brew_path = Some(host.to_path_buf());
            assert_eq!(find_brew(&cfg), None, "the host brew must stay off limits");
            cfg.require_sandbox = false;
            assert_eq!(find_brew(&cfg), Some(host.to_path_buf()));
        }
    }

    #[test]
    fn the_sandbox_guard_skips_the_path_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("elsewhere");
        let brew = bin.join("brew");
        executable(&brew);

        let mut cfg = Config::for_test(&dir.path().join("sandbox"));
        cfg.repository = cfg.prefix.clone();
        // SAFETY: single-threaded test; the variable is restored below.
        let old_path = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", &bin) };

        cfg.require_sandbox = false;
        assert_eq!(find_brew(&cfg), Some(brew));
        cfg.require_sandbox = true;
        assert_eq!(find_brew(&cfg), None);

        match old_path {
            // SAFETY: as above.
            Some(p) => unsafe { std::env::set_var("PATH", p) },
            None => unsafe { std::env::remove_var("PATH") },
        }
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
