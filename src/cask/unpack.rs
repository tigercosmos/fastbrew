//! Container extraction into the staged Caskroom directory: dmg (`hdiutil`),
//! zip (`ditto -x -k`), tar/tgz/tbz/txz (native), pkg (copied as-is), naked
//! binaries (copied), nested containers (`container_args[":nested"]`).
//!
//! Port of `Library/Homebrew/unpack_strategy.rb` and the `dmg`, `zip`, `tar`,
//! `pkg`, `uncompressed` and `directory` strategies, restricted to the
//! container kinds casks actually use.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::Config;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Dmg,
    Zip,
    Tar,
    Pkg,
    Naked,
}

/// Disk image metadata never copied out of a mounted volume
/// (`UnpackStrategy::Dmg::Bom::DMG_METADATA`).
pub const DMG_METADATA: &[&str] = &[
    ".background",
    ".com.apple.timemachine.donotpresent",
    ".com.apple.timemachine.supported",
    ".DocumentRevisions-V100",
    ".DS_Store",
    ".fseventsd",
    ".MobileBackups",
    ".Spotlight-V100",
    ".TemporaryItems",
    ".Trashes",
    ".VolumeIcon.icns",
    ".HFS+ Private Directory Data\r",
    ".HFS+ Private Data\r",
];

/// Directories a dmg alias may point at; those aliases are not copied.
const SYSTEM_DIRS: &[&str] = &[
    "/Applications",
    "/Applications/Utilities",
    "/Library",
    "/System",
    "/System/Library",
    "/usr",
    "/usr/bin",
    "/usr/local",
    "/usr/local/bin",
];

/// Pick a container from an explicit `container type:` or the file itself.
pub fn detect_container(download: &Path, type_override: Option<&str>) -> Container {
    if let Some(kind) = type_override {
        let kind = kind.strip_prefix(':').unwrap_or(kind);
        match kind {
            "dmg" => return Container::Dmg,
            "zip" => return Container::Zip,
            "tar" | "gzip" | "bzip2" | "xz" | "zstd" | "lzma" => return Container::Tar,
            "pkg" | "xar" => return Container::Pkg,
            "naked" | "nounzip" | "uncompressed" => return Container::Naked,
            _ => {}
        }
    }
    if let Some(from_magic) = magic_container(download) {
        return from_magic;
    }
    from_extension(&download.to_string_lossy()).unwrap_or(Container::Naked)
}

fn from_extension(name: &str) -> Option<Container> {
    let lower = name.to_ascii_lowercase();
    let ends = |exts: &[&str]| exts.iter().any(|e| lower.ends_with(e));
    if ends(&[".dmg"]) {
        return Some(Container::Dmg);
    }
    if ends(&[
        ".tar",
        ".tbz",
        ".tbz2",
        ".tar.bz2",
        ".tgz",
        ".tar.gz",
        ".tlzma",
        ".tar.lzma",
        ".txz",
        ".tar.xz",
        ".tar.zst",
    ]) {
        return Some(Container::Tar);
    }
    if ends(&[".zip", ".jar", ".xpi"]) {
        return Some(Container::Zip);
    }
    if ends(&[".pkg", ".mpkg"]) {
        return Some(Container::Pkg);
    }
    None
}

/// Magic-number detection (`UnpackStrategy.from_magic`), for the containers
/// casks use. Extension detection is the fallback.
fn magic_container(path: &Path) -> Option<Container> {
    let mut buf = [0u8; 512];
    let read = {
        use std::io::Read;
        let mut file = std::fs::File::open(path).ok()?;
        file.read(&mut buf).ok()?
    };
    let head = &buf[..read];
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        // `.pkg`/`.mpkg` are xar, not zip, so only the extension can rename a zip.
        return Some(match from_extension(&path.to_string_lossy()) {
            Some(Container::Pkg) => Container::Pkg,
            _ => Container::Zip,
        });
    }
    if head.starts_with(b"xar!") {
        return Some(Container::Pkg);
    }
    if read >= 262 && &head[257..262] == b"ustar" {
        return Some(Container::Tar);
    }
    if head.starts_with(&[0x1f, 0x8b]) || head.starts_with(b"BZh") || head.starts_with(b"\xfd7zXZ")
    {
        return Some(Container::Tar);
    }
    // `koly` trailer or UDIF/compressed dmg: fall back to the extension.
    None
}

/// Extract `download` into `dest`, unwrapping nested single-file archives the
/// way `UnpackStrategy#extract_nestedly` does.
pub fn unpack(
    cfg: &Config,
    download: &Path,
    container: Container,
    dest: &Path,
    verbose: bool,
) -> Result<()> {
    extract_nestedly(cfg, download, container, dest, verbose, 0)
}

/// Extract the cask's primary container, honoring `container_args`
/// (`:type` and `:nested`).
pub fn unpack_container_args(
    cfg: &Config,
    download: &Path,
    container_args: Option<&serde_json::Value>,
    dest: &Path,
    verbose: bool,
) -> Result<()> {
    let type_override = container_args
        .and_then(|c| c.get(":type"))
        .and_then(serde_json::Value::as_str);
    let nested = container_args
        .and_then(|c| c.get(":nested"))
        .and_then(serde_json::Value::as_str);
    let container = detect_container(download, type_override);

    let Some(nested) = nested else {
        return unpack(cfg, download, container, dest, verbose);
    };

    // `Cask::Download#extract_primary_container`: unpack once into a temporary
    // directory, then unpack the named inner container into the staged path.
    let tmp = temp_dir(cfg, "cask-installer")?;
    extract(cfg, download, container, tmp.path(), verbose)?;
    let inner = tmp.path().join(nested);
    if !inner.exists() {
        return Err(Error::user(format!(
            "Nested container '{nested}' is missing from the download."
        )));
    }
    make_tree_writable(tmp.path());
    let inner_container = detect_container(&inner, None);
    extract_nestedly(cfg, &inner, inner_container, dest, verbose, 0)
}

fn extract_nestedly(
    cfg: &Config,
    download: &Path,
    container: Container,
    dest: &Path,
    verbose: bool,
    depth: usize,
) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    if depth > 8 {
        return Err(Error::user(
            "Refusing to unpack more than 8 levels of nested containers.".to_string(),
        ));
    }
    // `UnpackStrategy::Uncompressed#extract_nestedly` (and `Pkg`, its subclass)
    // never unwrap: the file is the artifact.
    if matches!(container, Container::Pkg | Container::Naked) {
        return extract(cfg, download, container, dest, verbose);
    }

    let tmp = temp_dir(cfg, "homebrew-unpack")?;
    extract(cfg, download, container, tmp.path(), verbose)?;

    let mut children: Vec<PathBuf> = std::fs::read_dir(tmp.path())?
        .flatten()
        .map(|e| e.path())
        .collect();
    children.sort();

    if children.len() == 1 {
        let only = &children[0];
        let is_dir = std::fs::symlink_metadata(only)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if !is_dir {
            let inner = detect_container(only, None);
            if inner != Container::Naked {
                return extract_nestedly(cfg, only, inner, dest, verbose, depth + 1);
            }
        }
    }

    make_dirs_writable(tmp.path());
    for child in children {
        let target = dest.join(child.file_name().unwrap_or_default());
        move_path(&child, &target)?;
    }
    Ok(())
}

/// One extraction step, without nesting.
fn extract(
    cfg: &Config,
    download: &Path,
    container: Container,
    dest: &Path,
    verbose: bool,
) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    match container {
        Container::Dmg => extract_dmg(cfg, download, dest, verbose),
        Container::Zip => extract_zip(download, dest, verbose),
        Container::Tar => extract_tar(download, dest),
        Container::Pkg | Container::Naked => {
            let name = super::download::cached_basename(download);
            let target = dest.join(name);
            copy_path(download, &target)
        }
    }
}

// ---------------------------------------------------------------- dmg

/// Mount points reported by `hdiutil attach -plist`.
fn plist_mount_points(stdout: &[u8]) -> Vec<PathBuf> {
    let Ok(value) = plist::Value::from_reader_xml(std::io::Cursor::new(stdout)) else {
        return vec![];
    };
    let Some(dict) = value.as_dictionary() else {
        return vec![];
    };
    let Some(entities) = dict.get("system-entities").and_then(plist::Value::as_array) else {
        return vec![];
    };
    entities
        .iter()
        .filter_map(|e| e.as_dictionary())
        .filter_map(|e| e.get("mount-point"))
        .filter_map(plist::Value::as_string)
        .map(PathBuf::from)
        .collect()
}

fn extract_dmg(cfg: &Config, download: &Path, dest: &Path, verbose: bool) -> Result<()> {
    let mount_root = temp_dir(cfg, "homebrew-dmg")?;

    // `hdiutil attach ... ` with `qn\n` on stdin declines any EULA prompt.
    let attach = |image: &Path| -> std::io::Result<std::process::Output> {
        let mut child = Command::new("hdiutil")
            .args(["attach", "-plist", "-nobrowse", "-readonly", "-mountrandom"])
            .arg(mount_root.path())
            .arg(image)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(b"qn\n");
        }
        child.wait_with_output()
    };

    let first = attach(download)?;
    let mounts = if first.status.success() {
        plist_mount_points(&first.stdout)
    } else {
        if first.stdout.is_empty() {
            return Err(Error::user(format!(
                "Failed to mount '{}': {}",
                download.display(),
                String::from_utf8_lossy(&first.stderr).trim()
            )));
        }
        // A EULA was printed instead of a plist: convert and attach the copy.
        let cdr = mount_root.path().join(
            download
                .file_stem()
                .map(|s| format!("{}.cdr", s.to_string_lossy()))
                .unwrap_or_else(|| "image.cdr".to_string()),
        );
        let mut convert = Command::new("hdiutil");
        convert.args(["convert"]);
        if !verbose {
            convert.arg("-quiet");
        }
        convert
            .args(["-format", "UDTO", "-o"])
            .arg(&cdr)
            .arg(download);
        let out = convert.output()?;
        if !out.status.success() {
            return Err(Error::user(format!(
                "Failed to convert '{}': {}",
                download.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        // `hdiutil convert -format UDTO` appends `.dmg` to the output name.
        let cdr = if cdr.exists() {
            cdr
        } else {
            PathBuf::from(format!("{}.dmg", cdr.display()))
        };
        let second = attach(&cdr)?;
        if !second.status.success() {
            return Err(Error::user(format!(
                "Failed to mount '{}': {}",
                download.display(),
                String::from_utf8_lossy(&second.stderr).trim()
            )));
        }
        plist_mount_points(&second.stdout)
    };

    if mounts.is_empty() {
        return Err(Error::user(format!(
            "No mounts found in '{}'; perhaps this is a bad disk image?",
            download.display()
        )));
    }

    let copy = (|| -> Result<()> {
        for mount in &mounts {
            for entry in std::fs::read_dir(mount)?.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy().to_string();
                if DMG_METADATA.contains(&name.as_str()) {
                    continue;
                }
                if is_system_dir_symlink(&entry.path()) {
                    continue;
                }
                let target = dest.join(&name);
                let status = Command::new("ditto")
                    .arg("--")
                    .arg(entry.path())
                    .arg(&target)
                    .status()?;
                if !status.success() {
                    return Err(Error::user(format!(
                        "Failed to copy '{}' out of the disk image.",
                        entry.path().display()
                    )));
                }
            }
        }
        make_tree_writable(dest);
        Ok(())
    })();

    for mount in &mounts {
        let _ = Command::new("hdiutil")
            .args(["detach", "-force"])
            .arg(mount)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    copy
}

/// `Bom.system_dir_symlink?`: a dmg alias pointing at `/Applications` and friends.
fn is_system_dir_symlink(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_symlink() {
        return false;
    }
    let Ok(target) = std::fs::read_link(path) else {
        return false;
    };
    let resolved = if target.is_absolute() {
        target
    } else {
        path.parent().unwrap_or(Path::new("/")).join(target)
    };
    let resolved = resolved.to_string_lossy().trim_end_matches('/').to_string();
    SYSTEM_DIRS.contains(&resolved.as_str())
}

// ---------------------------------------------------------------- zip

fn extract_zip(download: &Path, dest: &Path, verbose: bool) -> Result<()> {
    // `ditto` keeps resource forks and Finder attributes, which `unzip` drops.
    let mut cmd = Command::new("ditto");
    cmd.args(["-x", "-k", "--sequesterRsrc", "--rsrc"])
        .arg(download)
        .arg(dest);
    if !verbose {
        cmd.stdout(Stdio::null());
    }
    let status = cmd.status()?;
    if !status.success() {
        return Err(Error::user(format!(
            "Failed to unzip '{}'.",
            download.display()
        )));
    }
    let _ = std::fs::remove_dir_all(dest.join("__MACOSX"));
    Ok(())
}

// ---------------------------------------------------------------- tar

fn extract_tar(download: &Path, dest: &Path) -> Result<()> {
    let name = download.to_string_lossy().to_ascii_lowercase();
    let gzipped = name.ends_with(".gz") || name.ends_with(".tgz") || {
        let mut head = [0u8; 2];
        use std::io::Read;
        std::fs::File::open(download)
            .and_then(|mut f| f.read_exact(&mut head))
            .is_ok()
            && head == [0x1f, 0x8b]
    };
    let plain = {
        let mut head = [0u8; 262];
        use std::io::Read;
        std::fs::File::open(download)
            .and_then(|mut f| f.read_exact(&mut head))
            .map(|()| &head[257..262] == b"ustar")
            .unwrap_or(false)
    };

    let file = std::fs::File::open(download)?;
    if gzipped {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
        archive.set_overwrite(true);
        archive.set_preserve_permissions(true);
        archive.unpack(dest)?;
        return Ok(());
    }
    if plain {
        let mut archive = tar::Archive::new(file);
        archive.set_overwrite(true);
        archive.set_preserve_permissions(true);
        archive.unpack(dest)?;
        return Ok(());
    }
    // bzip2/xz/zstd: `tar` on macOS handles every compression libarchive knows.
    let status = Command::new("/usr/bin/tar")
        .arg("--extract")
        .arg("--no-same-owner")
        .arg("--file")
        .arg(download)
        .arg("--directory")
        .arg(dest)
        .status()?;
    if !status.success() {
        return Err(Error::user(format!(
            "Failed to extract '{}'.",
            download.display()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------- helpers

fn temp_dir(cfg: &Config, prefix: &str) -> Result<tempfile::TempDir> {
    std::fs::create_dir_all(&cfg.temp)?;
    Ok(tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&cfg.temp)?)
}

/// `FileUtils.chmod "u+w"` over a staged tree, skipping symlinks.
pub fn make_tree_writable(root: &Path) {
    chmod_user_write(root, false);
}

/// `UnpackStrategy#each_directory { chmod "u+w" }`: directories only.
pub fn make_dirs_writable(root: &Path) {
    chmod_user_write(root, true);
}

fn chmod_user_write(root: &Path, directories_only: bool) {
    use std::os::unix::fs::PermissionsExt;
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        if entry.path_is_symlink() {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if directories_only && !metadata.is_dir() {
            continue;
        }
        let mode = metadata.permissions().mode();
        if mode & 0o200 == 0 {
            let _ = std::fs::set_permissions(
                entry.path(),
                std::fs::Permissions::from_mode(mode | 0o200),
            );
        }
    }
}

/// Rename `src` to `dst`, falling back to a recursive copy across devices.
pub fn move_path(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() || dst.is_symlink() {
        remove_path(dst)?;
    }
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    copy_path(src, dst)?;
    remove_path(src)
}

/// Copy a file, symlink or directory tree preserving modes and links.
pub fn copy_path(src: &Path, dst: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(src)?;
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(src)?;
        let _ = std::fs::remove_file(dst);
        std::os::unix::fs::symlink(target, dst)?;
        return Ok(());
    }
    if metadata.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)?.flatten() {
            copy_path(&entry.path(), &dst.join(entry.file_name()))?;
        }
        let _ = std::fs::set_permissions(dst, metadata.permissions());
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

/// `rm -rf` for a file, symlink or directory.
pub fn remove_path(path: &Path) -> Result<()> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension_and_override() {
        let tmp = tempfile::tempdir().unwrap();
        let dmg = tmp.path().join("Ghostty.dmg");
        std::fs::write(&dmg, b"not really a dmg").unwrap();
        assert_eq!(detect_container(&dmg, None), Container::Dmg);
        assert_eq!(detect_container(&dmg, Some(":naked")), Container::Naked);

        let zip = tmp.path().join("Demo.zip");
        std::fs::write(&zip, b"PK\x03\x04rest").unwrap();
        assert_eq!(detect_container(&zip, None), Container::Zip);

        let pkg = tmp.path().join("Thing.pkg");
        std::fs::write(&pkg, b"xar!rest").unwrap();
        assert_eq!(detect_container(&pkg, None), Container::Pkg);

        let unknown = tmp.path().join("binary");
        std::fs::write(&unknown, b"\x7fELF").unwrap();
        assert_eq!(detect_container(&unknown, None), Container::Naked);
    }

    #[test]
    fn parses_hdiutil_plist() {
        let plist = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict><key>system-entities</key><array>
  <dict><key>dev-entry</key><string>/dev/disk4</string></dict>
  <dict><key>mount-point</key><string>/private/tmp/x/dmg.123</string></dict>
</array></dict>
</plist>"#;
        assert_eq!(
            plist_mount_points(plist),
            vec![PathBuf::from("/private/tmp/x/dmg.123")]
        );
    }

    #[test]
    fn unpacks_a_tar_gz_with_a_single_top_level_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::cask::tests_support::config(tmp.path());
        let src = tmp.path().join("src/Demo.app/Contents/MacOS");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("demo"), b"#!/bin/sh\n").unwrap();

        let archive_path = tmp.path().join("demo.tar.gz");
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            builder
                .append_dir_all("Demo.app", tmp.path().join("src/Demo.app"))
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }

        let dest = tmp.path().join("staged");
        unpack(&cfg, &archive_path, Container::Tar, &dest, false).unwrap();
        assert!(dest.join("Demo.app/Contents/MacOS/demo").is_file());
    }

    #[test]
    fn naked_container_copies_the_download_under_its_real_name() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::cask::tests_support::config(tmp.path());
        let digest = "a".repeat(64);
        let download = tmp.path().join(format!("{digest}--Thing.pkg"));
        std::fs::write(&download, b"xar!payload").unwrap();

        let dest = tmp.path().join("staged");
        unpack(&cfg, &download, Container::Naked, &dest, false).unwrap();
        assert!(dest.join("Thing.pkg").is_file());
    }
}
