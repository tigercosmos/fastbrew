//! Extract a bottle tarball into the Cellar.
//!
//! The archive contains `<name>/<version>/...`. Extract into a temporary
//! directory inside the rack (`$CELLAR/<name>/.fastbrew-<random>`), then
//! rename `<tmp>/<name>/<version>` to `$CELLAR/<name>/<version>`. Preserve
//! modes, mtimes, symlinks and hard links. Fail if the keg already exists
//! unless `replace` (used by `reinstall`, and by an `upgrade` pouring over the
//! version already in the rack), in which case the old keg is moved aside to
//! `<version>.reinstall` and kept until the caller reports that the install
//! finished (`Homebrew::Reinstall.backup`/`restore_backup`).
//!
//! Homebrew does this with `tar --extract --file <bottle> --directory <tmp>`
//! followed by `FileUtils.mv` (`formula_installer.rb#pour`, `Bottle#stage`);
//! decompressing in-process avoids the `tar` and `gzip` spawns and lets
//! several bottles extract in parallel.
//!
//! Nothing an archive carries may write outside the staging directory:
//! [`unpack`] refuses `..` components, absolute paths and entries whose parent
//! resolves outside it, and the keg root itself has to be a real directory
//! (`lstat`, not `stat`) before it is renamed into the Cellar.

use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};

/// Suffix `Homebrew::Reinstall.backup_path` gives a keg it moves aside.
const BACKUP_SUFFIX: &str = ".reinstall";

/// The keg a replacing pour displaced, kept until the install finishes.
///
/// `Homebrew::Reinstall` moves the old keg to `<keg>.reinstall` before the
/// replacement is installed, puts it back when anything between the pour and
/// the last finishing step raises, and removes it only once `finish` returned.
/// Deleting it as soon as the new keg is renamed into place would leave
/// nothing at all behind when relocation or finishing then failed.
#[derive(Debug)]
pub struct Backup {
    keg: PathBuf,
    path: PathBuf,
}

impl Backup {
    /// `$CELLAR/<name>/<version>.reinstall`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `Homebrew::Reinstall.restore_backup` without the relink (only the
    /// caller knows whether the keg was linked): drop whatever is at the keg
    /// path now and move the backup back onto it.
    pub fn restore(&self) -> Result<()> {
        if std::fs::symlink_metadata(&self.path).is_err() {
            return Ok(());
        }
        remove_path(&self.keg);
        std::fs::rename(&self.path, &self.keg).map_err(|e| {
            Error::Other(anyhow::Error::new(e).context(format!(
                "restoring {} from {}",
                self.keg.display(),
                self.path.display()
            )))
        })
    }

    /// The success branch of `Reinstall.reinstall_formula`: the old keg goes.
    pub fn discard(&self) {
        remove_path(&self.path);
    }
}

/// What one pour left in the rack.
#[derive(Debug)]
pub struct Pour {
    /// `$CELLAR/<name>/<pkg_version>`.
    pub keg: PathBuf,
    /// The keg this pour replaced, or `None` when the rack held none.
    pub backup: Option<Backup>,
}

/// Extract `tarball` into `$CELLAR/<name>/<pkg_version>`.
pub fn extract_bottle(
    cfg: &Config,
    tarball: &Path,
    name: &str,
    pkg_version: &str,
    replace: bool,
) -> Result<Pour> {
    let rack = cfg.rack(name);
    let keg = rack.join(pkg_version);
    let occupied = std::fs::symlink_metadata(&keg).is_ok();
    if occupied && !replace {
        return Err(Error::user(format!(
            "Cannot install {name} {pkg_version}: {} already exists",
            keg.display()
        )));
    }
    std::fs::create_dir_all(&rack)?;

    let staging = Staging::new(&rack)?;
    let unpacked = staging.path().join(name).join(pkg_version);
    unpack(tarball, staging.path())?;
    check_promotable(staging.path(), &unpacked, tarball, name, pkg_version)?;

    // Move the old keg aside first: a rename onto a non-empty directory fails,
    // and a removal before the new keg is in place would leave nothing behind
    // if the rename failed.
    let displaced = if occupied {
        let path = backup_path(&keg);
        // A backup a killed run left behind would block the rename.
        remove_path(&path);
        std::fs::rename(&keg, &path)?;
        Some(Backup {
            keg: keg.clone(),
            path,
        })
    } else {
        None
    };
    if let Err(e) = std::fs::rename(&unpacked, &keg) {
        if let Some(backup) = &displaced {
            let _ = backup.restore();
        }
        return Err(Error::Other(anyhow::Error::new(e).context(format!(
            "moving the extracted bottle into {}",
            keg.display()
        ))));
    }
    Ok(Pour {
        keg,
        backup: displaced,
    })
}

/// `Homebrew::Reinstall.backup_path`.
pub fn backup_path(keg: &Path) -> PathBuf {
    let mut path = keg.as_os_str().to_os_string();
    path.push(BACKUP_SUFFIX);
    PathBuf::from(path)
}

/// Whether a rack entry is a reinstall backup rather than an installed keg.
pub fn is_backup_name(name: &str) -> bool {
    name.ends_with(BACKUP_SUFFIX)
}

/// Remove a file, symlink or directory, ignoring what is not there.
fn remove_path(path: &Path) {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return;
    };
    if md.file_type().is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// The keg root has to be a real directory inside the staging directory.
///
/// `Path::is_dir` follows symlinks, so a bottle whose `<name>/<version>` entry
/// is a symlink to a directory elsewhere would pass that test and then be
/// renamed into the Cellar as the symlink itself — every later write, the
/// receipt included, would land outside the prefix. Check every component
/// below the staging directory with `lstat`, and make the canonical path prove
/// it too, before anything is promoted.
fn check_promotable(
    staging: &Path,
    unpacked: &Path,
    tarball: &Path,
    name: &str,
    pkg_version: &str,
) -> Result<()> {
    let missing = || {
        Error::user(format!(
            "{} does not contain {name}/{pkg_version}",
            tarball.display()
        ))
    };
    let escape = || {
        Error::user(format!(
            "Cannot install {name} {pkg_version}: the {name}/{pkg_version} entry of {} is not a \
             directory inside the bottle, so the keg would land outside the Cellar",
            tarball.display()
        ))
    };

    let relative = unpacked.strip_prefix(staging).map_err(|_| escape())?;
    let mut current = staging.to_path_buf();
    for part in relative.components() {
        current.push(part);
        let Ok(md) = std::fs::symlink_metadata(&current) else {
            return Err(missing());
        };
        if md.file_type().is_symlink() {
            return Err(escape());
        }
        if !md.file_type().is_dir() {
            return Err(missing());
        }
    }
    let (Ok(root), Ok(canonical)) = (staging.canonicalize(), unpacked.canonicalize()) else {
        return Err(escape());
    };
    if !canonical.starts_with(&root) {
        return Err(escape());
    }
    Ok(())
}

/// Unpack a gzipped tarball into `dest`, preserving modes, mtimes, symlinks
/// and hard links, and refusing anything that would write outside `dest`.
///
/// `tar::Archive::unpack` *skips* an entry whose path contains `..` and
/// silently strips a leading `/`; a bottle carries neither, so both mean the
/// archive is trying to write somewhere it must not and the pour stops.
/// Driving the entries by hand keeps `Entry::unpack_in`'s own protections — a
/// parent directory that canonicalises outside `dest` (an entry written
/// through a symlink pointing elsewhere) and a hard link whose source is
/// outside `dest` are both errors there — and adds the refusal on top.
///
/// A symlink entry may still point anywhere: bottled kegs legitimately carry
/// absolute symlinks into `@@HOMEBREW_PREFIX@@`, and it is following one that
/// the checks above prevent.
pub fn unpack(tarball: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(tarball).map_err(|e| {
        Error::Other(anyhow::Error::new(e).context(format!("opening {}", tarball.display())))
    })?;
    let reader: Box<dyn Read> = if is_gzip(tarball) {
        Box::new(flate2::read::GzDecoder::new(BufReader::with_capacity(
            256 * 1024,
            file,
        )))
    } else {
        Box::new(BufReader::with_capacity(256 * 1024, file))
    };
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.set_overwrite(true);
    archive.set_unpack_xattrs(true);

    std::fs::create_dir_all(dest).map_err(|e| extracting(tarball, e))?;
    // `Archive::unpack` canonicalises the destination before handing it to
    // every entry; do the same, so the checks compare like with like.
    let root = dest.canonicalize().map_err(|e| extracting(tarball, e))?;

    let mut directories = Vec::new();
    for entry in archive.entries().map_err(|e| extracting(tarball, e))? {
        let mut entry = entry.map_err(|e| extracting(tarball, e))?;
        let path = entry
            .path()
            .map_err(|e| extracting(tarball, e))?
            .into_owned();
        check_entry_path(tarball, &path)?;
        if entry.header().entry_type().is_dir() {
            directories.push(entry);
            continue;
        }
        if !entry.unpack_in(&root).map_err(|e| extracting(tarball, e))? {
            return Err(escaping_entry(tarball, &path));
        }
    }
    // `Archive::_unpack` applies the directories last, deepest first, so their
    // modes cannot keep their own contents from being written.
    directories.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
    for mut dir in directories {
        let path = dir.path().map_err(|e| extracting(tarball, e))?.into_owned();
        if !dir.unpack_in(&root).map_err(|e| extracting(tarball, e))? {
            return Err(escaping_entry(tarball, &path));
        }
    }
    Ok(())
}

fn extracting(tarball: &Path, e: std::io::Error) -> Error {
    Error::Other(anyhow::Error::new(e).context(format!("extracting {}", tarball.display())))
}

fn escaping_entry(tarball: &Path, path: &Path) -> Error {
    Error::user(format!(
        "Cannot extract {}: {} points outside the archive",
        tarball.display(),
        path.display()
    ))
}

/// Refuse an entry whose own path leaves the archive root.
fn check_entry_path(tarball: &Path, path: &Path) -> Result<()> {
    for part in path.components() {
        match part {
            Component::CurDir | Component::Normal(_) => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(escaping_entry(tarball, path));
            }
        }
    }
    Ok(())
}

fn is_gzip(path: &Path) -> bool {
    let mut magic = [0u8; 2];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && magic == [0x1f, 0x8b]
}

/// A temporary directory inside the rack, removed on drop unless kept.
struct Staging {
    path: PathBuf,
}

impl Staging {
    fn new(rack: &Path) -> Result<Staging> {
        let path = unique_path(rack, ".fastbrew-");
        std::fs::create_dir(&path)?;
        Ok(Staging { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn unique_path(dir: &Path, prefix: &str) -> PathBuf {
    dir.join(format!("{prefix}{}", uuid::Uuid::new_v4().simple()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    /// Build `<name>/<version>/{bin/run,lib/link,lib/real,lib/hard}` as a
    /// gzipped tar, with the same entry kinds a real bottle carries.
    fn make_bottle(dir: &Path, name: &str, version: &str, body: &str) -> PathBuf {
        let root = format!("{name}/{version}");
        let tarball = dir.join(format!("{name}--{version}.tar.gz"));
        let out = std::fs::File::create(&tarball).unwrap();
        let enc = flate2::write::GzEncoder::new(out, flate2::Compression::fast());
        let mut builder = tar::Builder::new(enc);

        let mut dir_header = |path: &str| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_mode(0o755);
            h.set_size(0);
            h.set_mtime(1_700_000_000);
            h.set_cksum();
            builder.append_data(&mut h, path, std::io::empty()).unwrap();
        };
        dir_header(&format!("{name}/"));
        dir_header(&format!("{root}/"));
        dir_header(&format!("{root}/bin/"));
        dir_header(&format!("{root}/lib/"));

        let mut file = |path: String, mode: u32, content: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mode(mode);
            h.set_size(content.len() as u64);
            h.set_mtime(1_700_000_000);
            h.set_cksum();
            builder.append_data(&mut h, path, content).unwrap();
        };
        file(format!("{root}/bin/run"), 0o555, body.as_bytes());
        file(format!("{root}/lib/real"), 0o644, b"shared body");

        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_mode(0o777);
        h.set_size(0);
        h.set_mtime(1_700_000_000);
        builder
            .append_link(&mut h, format!("{root}/lib/link"), "../bin/run")
            .unwrap();

        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Link);
        h.set_mode(0o644);
        h.set_size(0);
        h.set_mtime(1_700_000_000);
        builder
            .append_link(
                &mut h,
                format!("{root}/lib/hard"),
                format!("{root}/lib/real"),
            )
            .unwrap();

        builder
            .into_inner()
            .unwrap()
            .finish()
            .unwrap()
            .flush()
            .unwrap();
        tarball
    }

    /// A tarball built from raw entries, for the archives a bottle never is.
    fn make_raw(dir: &Path, file_name: &str, entries: &[(tar::EntryType, &str, &str)]) -> PathBuf {
        let tarball = dir.join(file_name);
        let out = std::fs::File::create(&tarball).unwrap();
        let enc = flate2::write::GzEncoder::new(out, flate2::Compression::fast());
        let mut builder = tar::Builder::new(enc);
        for (kind, path, link) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(*kind);
            h.set_mode(0o755);
            h.set_size(0);
            h.set_mtime(1_700_000_000);
            if *kind == tar::EntryType::Regular {
                h.set_size(link.len() as u64);
                h.set_cksum();
                builder.append_data(&mut h, path, link.as_bytes()).unwrap();
            } else if *kind == tar::EntryType::Directory {
                h.set_cksum();
                builder.append_data(&mut h, path, std::io::empty()).unwrap();
            } else {
                builder.append_link(&mut h, path, link).unwrap();
            }
        }
        builder
            .into_inner()
            .unwrap()
            .finish()
            .unwrap()
            .flush()
            .unwrap();
        tarball
    }

    #[test]
    fn extracts_preserving_modes_symlinks_and_hardlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let tarball = make_bottle(tmp.path(), "tree", "2.3.2", "#!/bin/sh\necho tree\n");

        let keg = extract_bottle(&cfg, &tarball, "tree", "2.3.2", false)
            .unwrap()
            .keg;
        assert_eq!(keg, cfg.cellar.join("tree/2.3.2"));
        let mode = std::fs::metadata(keg.join("bin/run"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o555);
        assert_eq!(
            std::fs::read_link(keg.join("lib/link")).unwrap(),
            Path::new("../bin/run")
        );
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(keg.join("lib/real")).unwrap().ino(),
            std::fs::metadata(keg.join("lib/hard")).unwrap().ino(),
            "hard link was not preserved"
        );
        // The staging directory is gone.
        let leftovers: Vec<_> = std::fs::read_dir(cfg.rack("tree"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["2.3.2".to_string()]);
    }

    #[test]
    fn refuses_an_existing_keg_unless_replacing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let first = make_bottle(tmp.path(), "tree", "2.3.2", "#!/bin/sh\necho one\n");
        extract_bottle(&cfg, &first, "tree", "2.3.2", false).unwrap();

        let err = extract_bottle(&cfg, &first, "tree", "2.3.2", false).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        let second_dir = tmp.path().join("second");
        std::fs::create_dir(&second_dir).unwrap();
        let second = make_bottle(&second_dir, "tree", "2.3.2", "#!/bin/sh\necho two\n");
        let pour = extract_bottle(&cfg, &second, "tree", "2.3.2", true).unwrap();
        assert!(
            std::fs::read_to_string(pour.keg.join("bin/run"))
                .unwrap()
                .contains("two")
        );
        // The keg it replaced is kept until the caller says it may go.
        let backup = pour.backup.as_ref().expect("the old keg was displaced");
        assert_eq!(backup.path(), cfg.rack("tree").join("2.3.2.reinstall"));
        assert!(
            std::fs::read_to_string(backup.path().join("bin/run"))
                .unwrap()
                .contains("one")
        );
        backup.discard();
        let leftovers: Vec<_> = std::fs::read_dir(cfg.rack("tree"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["2.3.2".to_string()]);
    }

    #[test]
    fn a_restored_backup_puts_the_previous_keg_back() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let first = make_bottle(tmp.path(), "tree", "2.3.2", "#!/bin/sh\necho one\n");
        extract_bottle(&cfg, &first, "tree", "2.3.2", false).unwrap();

        let second_dir = tmp.path().join("second");
        std::fs::create_dir(&second_dir).unwrap();
        let second = make_bottle(&second_dir, "tree", "2.3.2", "#!/bin/sh\necho two\n");
        let pour = extract_bottle(&cfg, &second, "tree", "2.3.2", true).unwrap();
        pour.backup.as_ref().unwrap().restore().unwrap();

        assert!(
            std::fs::read_to_string(pour.keg.join("bin/run"))
                .unwrap()
                .contains("one"),
            "the working keg is back"
        );
        let leftovers: Vec<_> = std::fs::read_dir(cfg.rack("tree"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["2.3.2".to_string()]);
    }

    #[test]
    fn a_mismatched_archive_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let tarball = make_bottle(tmp.path(), "tree", "2.3.2", "x");
        let err = extract_bottle(&cfg, &tarball, "tree", "9.9.9", false).unwrap_err();
        assert!(err.to_string().contains("does not contain"), "{err}");
        let leftovers = std::fs::read_dir(cfg.rack("tree"))
            .unwrap()
            .flatten()
            .count();
        assert_eq!(leftovers, 0);
    }

    /// A `<name>/<version>` symlink pointing at a directory outside the Cellar
    /// passes `is_dir`, and renaming it into the rack would make every later
    /// write — the receipt first of all — land there instead.
    #[test]
    fn refuses_a_symlinked_keg_root() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("INSTALL_RECEIPT.json"), "{\"keep\":true}").unwrap();

        let tarball = make_raw(
            tmp.path(),
            "escape.tar.gz",
            &[(
                tar::EntryType::Symlink,
                "tree/2.3.2",
                outside.to_str().unwrap(),
            )],
        );
        let err = extract_bottle(&cfg, &tarball, "tree", "2.3.2", false).unwrap_err();
        assert!(
            err.to_string().contains("would land outside the Cellar"),
            "{err}"
        );
        assert!(!cfg.rack("tree").join("2.3.2").exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("INSTALL_RECEIPT.json")).unwrap(),
            "{\"keep\":true}",
            "nothing outside the Cellar was touched"
        );
        assert_eq!(
            std::fs::read_dir(cfg.rack("tree"))
                .unwrap()
                .flatten()
                .count(),
            0
        );
    }

    /// One regular entry whose name is written straight into the header, so
    /// the archive can carry what `tar::Builder` refuses to spell.
    fn make_unchecked_path(dir: &Path, file_name: &str, path: &str) -> PathBuf {
        let tarball = dir.join(file_name);
        let out = std::fs::File::create(&tarball).unwrap();
        let enc = flate2::write::GzEncoder::new(out, flate2::Compression::fast());
        let mut builder = tar::Builder::new(enc);
        let body = b"owned";
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_mtime(1_700_000_000);
        h.set_size(body.len() as u64);
        let name = &mut h.as_gnu_mut().unwrap().name;
        name[..path.len()].copy_from_slice(path.as_bytes());
        h.set_cksum();
        builder.append(&h, &body[..]).unwrap();
        builder
            .into_inner()
            .unwrap()
            .finish()
            .unwrap()
            .flush()
            .unwrap();
        tarball
    }

    #[test]
    fn refuses_a_traversing_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        for (i, path) in ["tree/2.3.2/../../../../escaped", "/escaped"]
            .iter()
            .enumerate()
        {
            let tarball = make_unchecked_path(tmp.path(), &format!("traverse{i}.tar.gz"), path);
            let err = extract_bottle(&cfg, &tarball, "tree", "2.3.2", false).unwrap_err();
            assert!(
                err.to_string().contains("points outside the archive"),
                "{path}: {err}"
            );
        }
        assert!(!tmp.path().join("escaped").exists());
    }

    /// An entry written *through* a symlink that leaves the staging directory
    /// is what `Entry::unpack_in`'s canonicalised parent check catches.
    #[test]
    fn refuses_an_entry_written_through_an_escaping_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        let tarball = make_raw(
            tmp.path(),
            "through.tar.gz",
            &[
                (tar::EntryType::Directory, "tree/", ""),
                (
                    tar::EntryType::Symlink,
                    "tree/out",
                    outside.to_str().unwrap(),
                ),
                (tar::EntryType::Regular, "tree/out/planted", "owned"),
            ],
        );
        let err = extract_bottle(&cfg, &tarball, "tree", "2.3.2", false).unwrap_err();
        assert!(err.to_string().contains("extracting"), "{err}");
        assert!(!outside.join("planted").exists(), "nothing was planted");
    }

    /// A hard link whose source is outside the staging directory would put an
    /// outside inode into the keg; `Entry::unpack_in` refuses it.
    #[test]
    fn refuses_a_hard_link_to_an_outside_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let secret = tmp.path().join("secret");
        std::fs::write(&secret, "secret").unwrap();

        let tarball = make_raw(
            tmp.path(),
            "hardlink.tar.gz",
            &[
                (tar::EntryType::Directory, "tree/", ""),
                (tar::EntryType::Directory, "tree/2.3.2/", ""),
                (
                    tar::EntryType::Link,
                    "tree/2.3.2/stolen",
                    secret.to_str().unwrap(),
                ),
            ],
        );
        let err = extract_bottle(&cfg, &tarball, "tree", "2.3.2", false).unwrap_err();
        assert!(!err.to_string().is_empty());
        assert!(!cfg.rack("tree").join("2.3.2/stolen").exists());
    }
}
