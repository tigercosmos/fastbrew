//! `uninstall --cask [--zap]` (port of `Cask::Installer#uninstall` and `#zap`).

use crate::api::index::Index;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::CaskEntry;
use crate::output;

use super::artifacts::ArtifactOptions;
use super::config::CaskDirs;
use super::install;
use super::{InstalledCask, unpack};

pub fn uninstall_casks(
    cfg: &Config,
    index: &Index,
    tokens: &[String],
    zap: bool,
    force: bool,
) -> Result<()> {
    let mut errors: Vec<String> = Vec::new();
    for token in tokens {
        // An entry is nice to have (it carries the current artifact list) but a
        // cask that has left the API must still be removable.
        let entry = crate::resolve::resolve_cask(cfg, index, token).ok();
        let cask_token = entry
            .as_ref()
            .map(|c| c.token.clone())
            .unwrap_or_else(|| super::token_from_full_token(token).to_string());

        let Some(installed) = super::installed_cask(cfg, &cask_token) else {
            if force {
                continue;
            }
            errors.push(format!("Cask '{cask_token}' is not installed."));
            continue;
        };

        let _lock = crate::keg::lock::lock_cask(cfg, &cask_token)?;
        let dirs = CaskDirs::read_or_resolve(cfg, &installed.config_path(), &[]);
        if let Err(error) =
            uninstall_installed_cask(cfg, &dirs, &installed, entry.as_ref(), zap, force)
        {
            errors.push(error.to_string());
        }
    }
    match errors.first() {
        Some(first) => Err(Error::user(first.clone())),
        None => Ok(()),
    }
}

/// Uninstall one discovered Caskroom entry.
///
/// `entry` is the current API definition when one exists; the artifacts come
/// from the installed receipt whenever it records them, so fastbrew removes
/// exactly what it (or Homebrew) installed.
pub fn uninstall_installed_cask(
    cfg: &Config,
    dirs: &CaskDirs,
    installed: &InstalledCask,
    entry: Option<&CaskEntry>,
    zap: bool,
    force: bool,
) -> Result<()> {
    let specs = install::installed_specs(cfg, dirs, installed, entry);
    let ctx = install::installed_context(installed);
    let opts = ArtifactOptions {
        force,
        ..ArtifactOptions::default()
    };

    output::ohai(&format!("Uninstalling Cask {}", installed.token));
    if specs.is_empty() && entry.is_none() {
        output::opoo(&format!(
            "No uninstall artifact metadata is available for Cask '{}'.\nHomebrew will remove its records, but files installed by the Cask may remain.",
            installed.token
        ));
    }

    let result = super::artifacts::uninstall_specs(cfg, dirs, &specs, &ctx, zap, opts);
    if result.is_err() && !force {
        return result;
    }

    let metadata_dir = installed.metadata_main_container_path();
    let _ = std::fs::remove_file(installed.receipt_path());
    let _ = std::fs::remove_file(installed.config_path());
    let _ = std::fs::remove_file(installed.download_sha_path());

    if zap {
        output::ohai(&format!(
            "Removing all staged versions of Cask '{}'",
            installed.token
        ));
        let _ = unpack::remove_path(&installed.caskroom_path);
    } else {
        output::ohai(&format!(
            "Purging files for version {} of Cask {}",
            installed.version, installed.token
        ));
        let _ = unpack::remove_path(&installed.staged_path());
        let _ = std::fs::remove_dir_all(installed.metadata_versioned_path());
        let _ = std::fs::remove_dir(&metadata_dir);
        let _ = std::fs::remove_dir(&installed.caskroom_path);
        if force {
            let _ = unpack::remove_path(&installed.caskroom_path);
        }
    }
    remove_broken_caskroom_symlinks(cfg, &installed.token);
    result
}

/// `Cask::Installer#remove_broken_caskroom_symlinks`: drop rename symlinks the
/// removal has broken.
fn remove_broken_caskroom_symlinks(cfg: &Config, token: &str) {
    let Ok(entries) = std::fs::read_dir(cfg.caskroom()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_symlink() || path.exists() {
            continue;
        }
        let points_at_token = std::fs::read_link(&path)
            .ok()
            .and_then(|t| t.file_name().map(|n| n.to_string_lossy().into_owned()))
            .is_some_and(|name| name == token);
        if points_at_token {
            let _ = std::fs::remove_file(path);
        }
    }
}
