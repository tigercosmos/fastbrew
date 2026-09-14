//! Formula and cask locks: `var/homebrew/locks/<name>.formula.lock` with
//! `flock(LOCK_EX | LOCK_NB)`.
//!
//! Ported from `Library/Homebrew/lock_file.rb` (plus `lock_file/formula_lock.rb`
//! and `lock_file/cask_lock.rb`). The contention message is
//! `OperationInProgressError`'s (`exceptions.rb`):
//!
//! ```text
//! A `brew` process has already locked <locked path>.
//! Please wait for it to finish or terminate it to continue.
//! ```
//!
//! The locked path is the rack (`$CELLAR/<name>`) for formulae and
//! `$PREFIX/Caskroom/<token>` for casks; the lock file itself lives in
//! `$PREFIX/var/homebrew/locks`.

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};

/// A held `flock`. Dropping it releases the lock (the file is closed).
#[derive(Debug)]
pub struct Lock {
    _file: File,
    path: PathBuf,
}

impl Lock {
    /// Path of the lock file itself.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// `$PREFIX/var/homebrew/locks/<name>.formula.lock`, guarding `$CELLAR/<name>`.
pub fn lock_formula(cfg: &Config, name: &str) -> Result<Lock> {
    lock(cfg, name, "formula", &cfg.rack(name))
}

/// `$PREFIX/var/homebrew/locks/<token>.cask.lock`, guarding `$PREFIX/Caskroom/<token>`.
pub fn lock_cask(cfg: &Config, token: &str) -> Result<Lock> {
    lock(cfg, token, "cask", &cfg.caskroom().join(token))
}

fn lock(cfg: &Config, name: &str, kind: &str, locked_path: &Path) -> Result<Lock> {
    let dir = cfg.locks_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.{kind}.lock"));

    // `lock_file.rb` retries when the file it locked was unlinked between the
    // open and the flock, so that two processes never hold locks on files with
    // different inodes.
    for _ in 0..8 {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        set_cloexec(&file);
        // SAFETY: `fd` is a valid open descriptor for the lifetime of the call.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if !locked {
            return Err(Error::user(operation_in_progress_message(locked_path)));
        }
        let same_inode = match (file.metadata(), std::fs::metadata(&path)) {
            (Ok(a), Ok(b)) => {
                use std::os::unix::fs::MetadataExt;
                a.ino() == b.ino()
            }
            _ => false,
        };
        if same_inode {
            return Ok(Lock { _file: file, path });
        }
        // The file was replaced under us: drop this handle and try again.
    }
    Err(Error::user(operation_in_progress_message(locked_path)))
}

/// `OperationInProgressError#initialize` without the `HOMEBREW_LOCK_CONTEXT` extra.
pub fn operation_in_progress_message(locked_path: &Path) -> String {
    format!(
        "A `brew` process has already locked {}.\nPlease wait for it to finish or terminate it to continue.",
        locked_path.display()
    )
}

fn set_cloexec(file: &File) {
    // SAFETY: `fd` is valid; FD_CLOEXEC is the only flag we set.
    unsafe {
        let fd = file.as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_within_process() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let held = lock_formula(&cfg, "jq").unwrap();
        assert!(held.path().ends_with("jq.formula.lock"));
        // A second `flock(LOCK_EX|LOCK_NB)` on the same file from the same
        // process but a different descriptor fails just like another process.
        let err = lock_formula(&cfg, "jq").unwrap_err();
        assert!(
            err.to_string()
                .starts_with("A `brew` process has already locked "),
            "{err}"
        );
        assert!(err.to_string().contains("Cellar/jq"), "{err}");
        drop(held);
        // Released: taking it again works.
        let _again = lock_formula(&cfg, "jq").unwrap();
    }

    #[test]
    fn different_names_and_kinds_do_not_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let _a = lock_formula(&cfg, "jq").unwrap();
        let _b = lock_formula(&cfg, "oniguruma").unwrap();
        let c = lock_cask(&cfg, "jq").unwrap();
        assert!(c.path().ends_with("jq.cask.lock"));
    }
}
