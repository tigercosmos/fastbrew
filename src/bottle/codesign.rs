//! Ad-hoc re-signing of modified Mach-O files (`extend/os/mac/keg.rb`).
//!
//! `codesign --sign - --force --preserve-metadata=entitlements,requirements,flags,runtime <file>`;
//! on failure copy the file to a new inode and retry once (a known workaround
//! for a `codesign` bug). Run in parallel across files with `rayon`, making
//! each file writable first if needed (`Utils::Path.ensure_writable`).

use std::path::Path;
use std::process::Command;

use rayon::prelude::*;

use crate::error::Result;
use crate::output::onoe;

/// `/usr/bin/codesign` is used explicitly so a keg's own `codesign` on `PATH`
/// can never be picked up.
const CODESIGN: &str = "/usr/bin/codesign";

/// Re-sign every file, in parallel. Failures are reported like Homebrew's
/// `onoe` and do not abort the pour, matching `codesign_patched_binary`.
pub fn codesign_files(files: &[&Path]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(files.len());
    let run = || {
        files.par_iter().for_each(|file| {
            let _ = super::with_writable(file, || {
                codesign_one(file);
                Ok(())
            });
        });
    };
    // `Hardware::CPU.cores` bounds Homebrew's signing pool; bound ours the same
    // way rather than flooding the global rayon pool for a big keg.
    match rayon::ThreadPoolBuilder::new().num_threads(threads).build() {
        Ok(pool) => pool.install(run),
        Err(_) => run(),
    }
    Ok(())
}

/// Sign one file, retrying once through a fresh inode.
pub fn codesign_one(file: &Path) {
    if sign(file) {
        return;
    }
    // Known workaround for a `codesign` bug: copy the file to another inode,
    // then move it back over the original, and sign again.
    if copy_through_new_inode(file).is_ok() && sign(file) {
        return;
    }
    onoe(&format!(
        "Failed applying an ad-hoc signature to {}",
        file.display()
    ));
}

fn sign(file: &Path) -> bool {
    Command::new(CODESIGN)
        .args([
            "--sign",
            "-",
            "--force",
            "--preserve-metadata=entitlements,requirements,flags,runtime",
        ])
        .arg(file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn copy_through_new_inode(file: &Path) -> std::io::Result<()> {
    let dir = file.parent().unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::Builder::new()
        .prefix("workaround")
        .tempfile_in(dir)?;
    let mode = std::fs::metadata(file)?.permissions();
    std::fs::copy(file, tmp.path())?;
    std::fs::set_permissions(tmp.path(), mode)?;
    tmp.persist(file).map_err(|e| e.error)?;
    Ok(())
}

/// Whether `codesign --verify` accepts the file. Used by tests and by
/// diagnostics; signing itself never depends on it.
pub fn verify(file: &Path) -> bool {
    Command::new(CODESIGN)
        .arg("--verify")
        .arg(file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_a_non_macho_file_is_reported_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("plain.txt");
        std::fs::write(&p, b"not a mach-o").unwrap();
        // `codesign` fails on this; the call must still return Ok(()).
        codesign_files(&[&p]).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"not a mach-o");
    }

    #[test]
    fn read_only_files_are_made_writable_for_signing() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("ro.bin");
        std::fs::write(&p, b"x").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444)).unwrap();
        codesign_files(&[&p]).unwrap();
        // The original mode is restored afterwards.
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o444);
    }
}
