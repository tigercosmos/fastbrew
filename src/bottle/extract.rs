//! Extract a bottle tarball into the Cellar.
//!
//! The archive contains `<name>/<version>/...`. Extract into a temporary
//! directory inside the rack (`$CELLAR/<name>/.fastbrew-<random>`), then
//! rename `<tmp>/<name>/<version>` to `$CELLAR/<name>/<version>`. Preserve
//! modes, mtimes, symlinks and hard links. Fail if the keg already exists
//! unless `replace` (used by `reinstall`), in which case the old keg is
//! removed after a successful extraction.
//!
//! Homebrew does this with `tar --extract --file <bottle> --directory <tmp>`
//! followed by `FileUtils.mv` (`formula_installer.rb#pour`, `Bottle#stage`);
//! decompressing in-process avoids the `tar` and `gzip` spawns and lets
//! several bottles extract in parallel.

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};

/// Extracted keg path (`$CELLAR/<name>/<pkg_version>`).
pub fn extract_bottle(
    cfg: &Config,
    tarball: &Path,
    name: &str,
    pkg_version: &str,
    replace: bool,
) -> Result<PathBuf> {
    let rack = cfg.rack(name);
    let keg = rack.join(pkg_version);
    if keg.exists() && !replace {
        return Err(Error::user(format!(
            "Cannot install {name} {pkg_version}: {} already exists",
            keg.display()
        )));
    }
    std::fs::create_dir_all(&rack)?;

    let staging = Staging::new(&rack)?;
    let unpacked = staging.path().join(name).join(pkg_version);
    unpack(tarball, staging.path())?;
    if !unpacked.is_dir() {
        return Err(Error::user(format!(
            "{} does not contain {name}/{pkg_version}",
            tarball.display()
        )));
    }

    // Move the old keg aside first: a rename onto a non-empty directory fails,
    // and a removal before the new keg is in place would leave nothing behind
    // if the rename failed.
    let displaced = if keg.exists() {
        let old = unique_path(&rack, ".fastbrew-old-");
        std::fs::rename(&keg, &old)?;
        Some(old)
    } else {
        None
    };
    if let Err(e) = std::fs::rename(&unpacked, &keg) {
        if let Some(old) = displaced {
            let _ = std::fs::rename(&old, &keg);
        }
        return Err(Error::Other(anyhow::Error::new(e).context(format!(
            "moving the extracted bottle into {}",
            keg.display()
        ))));
    }
    if let Some(old) = displaced {
        let _ = std::fs::remove_dir_all(old);
    }
    Ok(keg)
}

/// Unpack a gzipped tarball into `dest`, preserving modes, mtimes, symlinks
/// and hard links.
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
    archive.unpack(dest).map_err(|e| {
        Error::Other(anyhow::Error::new(e).context(format!("extracting {}", tarball.display())))
    })?;
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

    #[test]
    fn extracts_preserving_modes_symlinks_and_hardlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let tarball = make_bottle(tmp.path(), "tree", "2.3.2", "#!/bin/sh\necho tree\n");

        let keg = extract_bottle(&cfg, &tarball, "tree", "2.3.2", false).unwrap();
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
        let keg = extract_bottle(&cfg, &second, "tree", "2.3.2", true).unwrap();
        assert!(
            std::fs::read_to_string(keg.join("bin/run"))
                .unwrap()
                .contains("two")
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
}
