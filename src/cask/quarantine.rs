//! `com.apple.quarantine` propagation (port of `Library/Homebrew/cask/quarantine.rb`).
//!
//! Homebrew never sets a quarantine attribute itself here: it reads the one
//! macOS attached to the download, flips the "no translocation" bit and writes
//! the result onto every staged file so the app runs from the Caskroom instead
//! of a randomized read-only mount.

use std::path::Path;

use crate::error::Result;

/// `Quarantine::QUARANTINE_ATTRIBUTE`.
pub const QUARANTINE_ATTRIBUTE: &str = "com.apple.quarantine";

/// `Quarantine::USER_APPROVED_FLAG` (`QuarantineSPI.h`).
pub const USER_APPROVED_FLAG: u64 = 0x0040;

/// Bit 8 of the flags field disables app translocation.
pub const NO_TRANSLOCATION_FLAG: u64 = 0x0100;

/// `Quarantine.status`: the raw attribute value, or `None` when absent.
pub fn status(path: &Path) -> Option<String> {
    let raw = xattr::get(path, QUARANTINE_ATTRIBUTE).ok().flatten()?;
    let text = String::from_utf8_lossy(&raw).trim_end().to_string();
    (!text.is_empty()).then_some(text)
}

/// `Quarantine.detect`: whether the path carries a quarantine attribute.
pub fn detect(path: &Path) -> bool {
    status(path).is_some()
}

/// `Quarantine.toggle_no_translocation_bit`: OR bit 8 into the flags field.
///
/// The attribute is `<flags>;<hex time>;<agent>;<uuid>`; only the first field
/// changes, and it keeps at least four hex digits.
pub fn toggle_no_translocation_bit(attribute: &str) -> String {
    let mut fields: Vec<String> = attribute.split(';').map(str::to_string).collect();
    if fields.is_empty() {
        return attribute.to_string();
    }
    let flags = u64::from_str_radix(fields[0].trim(), 16).unwrap_or(0);
    fields[0] = format!("{:0>4x}", flags | NO_TRANSLOCATION_FLAG);
    fields.join(";")
}

/// `Quarantine.user_approved_status?`: whether the flags carry the approval bit.
pub fn user_approved_status(attribute: &str) -> bool {
    if attribute.is_empty() {
        return false;
    }
    let flags = attribute
        .split(';')
        .next()
        .and_then(|f| u64::from_str_radix(f.trim(), 16).ok())
        .unwrap_or(0);
    flags & USER_APPROVED_FLAG != 0
}

/// `Quarantine.propagate`: copy `from`'s quarantine attribute (with the
/// no-translocation bit set) onto everything under `to`, making each path
/// writable first (`chmod -h u+w`).
pub fn propagate(from: &Path, to: &Path) -> Result<()> {
    let Some(attribute) = status(from) else {
        return Ok(());
    };
    let attribute = toggle_no_translocation_bit(&attribute);
    for path in staged_paths(to) {
        make_user_writable(&path);
        // Failures here are not fatal: a read-only staged file simply keeps the
        // attribute it already has, exactly like Homebrew's `xargs` best effort.
        let _ = xattr::set(&path, QUARANTINE_ATTRIBUTE, attribute.as_bytes());
    }
    Ok(())
}

/// `Pathname.glob(to/"**/*", File::FNM_DOTMATCH).reject(&:symlink?)`: every
/// descendant of `to` (not `to` itself), symlinks excluded.
fn staged_paths(to: &Path) -> Vec<std::path::PathBuf> {
    walkdir::WalkDir::new(to)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|e| !e.path_is_symlink())
        .map(|e| e.into_path())
        .collect()
}

/// `chmod -h u+w`, ignoring failures on paths we do not own.
fn make_user_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() {
        return;
    }
    let mode = metadata.permissions().mode();
    if mode & 0o200 != 0 {
        return;
    }
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o200));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_no_translocation_bit() {
        assert_eq!(
            toggle_no_translocation_bit("0081;68b8c1a2;Safari;1B2C3D4E"),
            "0181;68b8c1a2;Safari;1B2C3D4E"
        );
        // Already set: unchanged, but normalized to four digits.
        assert_eq!(
            toggle_no_translocation_bit("181;68b8c1a2;Safari;X"),
            "0181;68b8c1a2;Safari;X"
        );
        // Wide flags keep every digit.
        assert_eq!(toggle_no_translocation_bit("00020081;a;b;c"), "20181;a;b;c");
        assert_eq!(toggle_no_translocation_bit("0083"), "0183");
    }

    #[test]
    fn reads_user_approved_flag() {
        assert!(user_approved_status("0043;68b8c1a2;Homebrew;X"));
        assert!(!user_approved_status("0083;68b8c1a2;Homebrew;X"));
        assert!(!user_approved_status(""));
    }

    #[test]
    fn propagates_between_files() {
        let tmp = tempfile::tempdir().unwrap();
        let download = tmp.path().join("Demo.zip");
        std::fs::write(&download, b"x").unwrap();
        let staged = tmp.path().join("staged");
        std::fs::create_dir_all(staged.join("Demo.app/Contents")).unwrap();
        std::fs::write(staged.join("Demo.app/Contents/Info.plist"), b"y").unwrap();

        // No attribute on the download: nothing happens and it is not an error.
        propagate(&download, &staged).unwrap();
        assert!(status(&staged.join("Demo.app")).is_none());

        if xattr::set(&download, QUARANTINE_ATTRIBUTE, b"0083;68b8c1a2;Safari;ABC").is_err() {
            return; // The filesystem does not support extended attributes.
        }
        propagate(&download, &staged).unwrap();
        assert_eq!(
            status(&staged.join("Demo.app/Contents/Info.plist")).as_deref(),
            Some("0183;68b8c1a2;Safari;ABC")
        );
        // The destination root itself is not touched, matching the Ruby glob.
        assert!(status(&staged).is_none());
    }
}
