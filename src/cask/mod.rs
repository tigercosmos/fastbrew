//! Cask engine (`docs/DESIGN.md` 8, `docs/COMPAT.md` 6).
//!
//! Layout of an installed cask, reproducing `Library/Homebrew/cask/caskroom.rb`
//! and `cask/metadata.rb`:
//!
//! ```text
//! $PREFIX/Caskroom/<token>/<version>/                       staged files
//! $PREFIX/Caskroom/<token>/.metadata/config.json
//! $PREFIX/Caskroom/<token>/.metadata/INSTALL_RECEIPT.json
//! $PREFIX/Caskroom/<token>/.metadata/<version>/<timestamp>/Casks/<token>.json
//! ```

pub mod artifacts;
pub mod config;
pub mod download;
pub mod install;
pub mod metadata;
pub mod quarantine;
pub mod steps;
pub mod uninstall;
pub mod unpack;

use std::path::{Path, PathBuf};

use crate::config::Config;

/// `Cask::Metadata::METADATA_SUBDIR`.
pub const METADATA_SUBDIR: &str = ".metadata";

/// Caskfile extensions searched inside `.metadata/<version>/<timestamp>/Casks`,
/// in `Cask::Caskroom::CASKFILE_EXTENSIONS` order.
pub const CASKFILE_EXTENSIONS: &[&str] = &["json", "internal.json", "rb"];

/// Installed cask discovered in the Caskroom.
#[derive(Debug, Clone)]
pub struct InstalledCask {
    pub token: String,
    pub version: String,
    pub caskroom_path: PathBuf,
    /// `.metadata/<version>/<timestamp>` of the latest install.
    pub metadata_path: Option<PathBuf>,
}

impl InstalledCask {
    /// `<caskroom>/<version>`: the staged distribution.
    pub fn staged_path(&self) -> PathBuf {
        self.caskroom_path.join(&self.version)
    }

    /// `<caskroom>/.metadata`.
    pub fn metadata_main_container_path(&self) -> PathBuf {
        self.caskroom_path.join(METADATA_SUBDIR)
    }

    /// `<caskroom>/.metadata/<version>`.
    pub fn metadata_versioned_path(&self) -> PathBuf {
        self.metadata_main_container_path().join(&self.version)
    }

    pub fn config_path(&self) -> PathBuf {
        self.metadata_main_container_path().join("config.json")
    }

    pub fn receipt_path(&self) -> PathBuf {
        self.metadata_main_container_path()
            .join("INSTALL_RECEIPT.json")
    }

    pub fn download_sha_path(&self) -> PathBuf {
        self.metadata_main_container_path()
            .join("LATEST_DOWNLOAD_SHA256")
    }

    /// `.metadata/<version>/<timestamp>/Casks/<token>.{json,internal.json,rb}`.
    pub fn caskfile_path(&self) -> Option<PathBuf> {
        let dir = self.metadata_path.as_ref()?.join("Casks");
        CASKFILE_EXTENSIONS
            .iter()
            .map(|ext| dir.join(format!("{}.{ext}", self.token)))
            .find(|p| p.exists())
    }
}

/// `Caskroom.path/<token>`.
pub fn caskroom_path(cfg: &Config, token: &str) -> PathBuf {
    cfg.caskroom().join(token_from_full_token(token))
}

/// `<caskroom>/<version>`.
pub fn staged_path(cfg: &Config, token: &str, version: &str) -> PathBuf {
    caskroom_path(cfg, token).join(version)
}

/// `Caskroom.token_from_full_token`: `user/repo/token` -> `token`.
pub fn token_from_full_token(token: &str) -> &str {
    let mut parts = token.splitn(3, '/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(_), Some(_), Some(t)) => t,
        _ => token,
    }
}

/// `Cask::Metadata::TIMESTAMP_FORMAT` (`%Y%m%d%H%M%S.%L`, UTC).
pub fn new_timestamp() -> String {
    chrono::Utc::now().format("%Y%m%d%H%M%S%.3f").to_string()
}

/// Latest `.metadata/<version>/<timestamp>` directory of `caskroom_path`,
/// picked by the greatest timestamp basename (`metadata_timestamped_path`).
fn latest_timestamped_path(caskroom_path: &Path) -> Option<PathBuf> {
    let metadata = caskroom_path.join(METADATA_SUBDIR);
    let mut best: Option<PathBuf> = None;
    for version in std::fs::read_dir(&metadata).ok()?.flatten() {
        if !version.path().is_dir() {
            continue;
        }
        let Ok(stamps) = std::fs::read_dir(version.path()) else {
            continue;
        };
        for stamp in stamps.flatten() {
            if !stamp.path().is_dir() {
                continue;
            }
            let replace = match &best {
                None => true,
                Some(current) => stamp.file_name() > current.file_name().unwrap_or_default(),
            };
            if replace {
                best = Some(stamp.path());
            }
        }
    }
    best
}

/// Discover the installed cask in `caskroom_path`, or `None` when the
/// directory carries no caskfile (Homebrew treats it as not installed).
fn discover(caskroom_path: &Path) -> Option<InstalledCask> {
    let token = caskroom_path.file_name()?.to_str()?.to_string();
    let metadata_path = latest_timestamped_path(caskroom_path)?;
    let casks_dir = metadata_path.join("Casks");
    let found = CASKFILE_EXTENSIONS
        .iter()
        .map(|ext| casks_dir.join(format!("{token}.{ext}")))
        .any(|p| p.exists());
    if !found {
        return None;
    }
    // `.metadata/<version>/<timestamp>` -> `<version>`
    let version = metadata_path.parent()?.file_name()?.to_str()?.to_string();
    Some(InstalledCask {
        token,
        version,
        caskroom_path: caskroom_path.to_path_buf(),
        metadata_path: Some(metadata_path),
    })
}

pub fn installed_casks(cfg: &Config) -> Vec<InstalledCask> {
    let Ok(entries) = std::fs::read_dir(cfg.caskroom()) else {
        return vec![];
    };
    let mut out: Vec<InstalledCask> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false) && !e.path().is_symlink())
        .filter_map(|e| discover(&e.path()))
        .collect();
    out.sort_by(|a, b| a.token.cmp(&b.token));
    out
}

pub fn installed_cask(cfg: &Config, token: &str) -> Option<InstalledCask> {
    let path = caskroom_path(cfg, token);
    if !path.is_dir() || path.is_symlink() {
        return None;
    }
    discover(&path)
}

/// Shared fixtures for the unit tests of the cask modules.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    /// A `Config` rooted at `root` with no environment lookups.
    pub(crate) fn config(root: &Path) -> Config {
        Config {
            prefix: root.join("prefix"),
            cellar: root.join("prefix/Cellar"),
            repository: root.join("prefix"),
            library: root.join("prefix/Library"),
            cache: root.join("cache"),
            logs: root.join("logs"),
            temp: root.join("tmp"),
            home: root.join("home"),
            api_domain: crate::config::DEFAULT_API_DOMAIN.to_string(),
            bottle_domain: crate::config::DEFAULT_BOTTLE_DOMAIN.to_string(),
            artifact_domain: None,
            github_packages_token: None,
            github_packages_user: None,
            no_auto_update: true,
            auto_update_secs: 86_400,
            api_auto_update_secs: 450,
            no_install_cleanup: true,
            no_install_upgrade: false,
            no_installed_dependents_check: false,
            no_emoji: false,
            install_badge: "🍺".to_string(),
            no_env_hints: true,
            verbose: false,
            debug: false,
            download_concurrency: 1,
            cleanup_max_age_days: 120,
            curl_retries: 0,
            cask_opts: vec![],
            brew_path: None,
            no_delegate: true,
            require_sandbox: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tap_from_token() {
        assert_eq!(token_from_full_token("ghostty"), "ghostty");
        assert_eq!(token_from_full_token("homebrew/cask/ghostty"), "ghostty");
        assert_eq!(token_from_full_token("user/repo/my-cask"), "my-cask");
    }

    #[test]
    fn timestamp_shape() {
        let stamp = new_timestamp();
        assert_eq!(stamp.len(), 18, "{stamp}");
        assert_eq!(&stamp[14..15], ".");
        assert!(stamp[..14].chars().all(|c| c.is_ascii_digit()));
        assert!(stamp[15..].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn discovers_installed_cask() {
        let tmp = tempfile::tempdir().unwrap();
        let caskroom = tmp.path().join("Caskroom/demo");
        let meta = caskroom.join(".metadata/1.0/20250101000000.123/Casks");
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(meta.join("demo.json"), "{}").unwrap();
        std::fs::create_dir_all(caskroom.join("1.0")).unwrap();

        let found = discover(&caskroom).unwrap();
        assert_eq!(found.token, "demo");
        assert_eq!(found.version, "1.0");
        assert_eq!(found.staged_path(), caskroom.join("1.0"));
        assert_eq!(
            found.caskfile_path().unwrap(),
            caskroom.join(".metadata/1.0/20250101000000.123/Casks/demo.json")
        );
    }

    #[test]
    fn picks_latest_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let caskroom = tmp.path().join("Caskroom/demo");
        for (version, stamp) in [("1.0", "20250101000000.123"), ("2.0", "20250301000000.001")] {
            let meta = caskroom.join(format!(".metadata/{version}/{stamp}/Casks"));
            std::fs::create_dir_all(&meta).unwrap();
            std::fs::write(meta.join("demo.json"), "{}").unwrap();
        }
        assert_eq!(discover(&caskroom).unwrap().version, "2.0");
    }
}
