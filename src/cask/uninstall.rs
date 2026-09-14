//! `uninstall --cask [--zap]` (port of `Cask::Installer#uninstall` and `#zap`).

use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::CaskEntry;
use crate::output;

use super::artifacts::ArtifactOptions;
use super::config::CaskDirs;
use super::install;
use super::{InstalledCask, unpack};

/// Flags of one `brew uninstall --cask` run.
#[derive(Debug, Clone, Copy, Default)]
pub struct CaskUninstallOptions {
    /// `--zap`: reverse the artifacts and remove every staged version.
    pub zap: bool,
    /// `--force`: keep going when a removal fails, and remove an uninstalled cask.
    pub force: bool,
    /// `--dry-run`: only list what would be removed (`cmd/uninstall.rb`).
    pub dry_run: bool,
}

pub fn uninstall_casks(
    cfg: &Config,
    casks: &[CaskEntry],
    opts: CaskUninstallOptions,
) -> Result<()> {
    let mut errors: Vec<String> = Vec::new();
    for entry in casks {
        // The resolved entry carries the current artifact list; a cask that has
        // left the API resolves from the Caskroom, so it stays removable.
        let cask_token = entry.token.clone();

        let Some(installed) = super::installed_cask(cfg, &cask_token) else {
            if opts.force {
                continue;
            }
            errors.push(format!("Cask '{cask_token}' is not installed."));
            continue;
        };

        // A dry run changes nothing, so it needs no lock.
        let _lock = if opts.dry_run {
            None
        } else {
            Some(crate::keg::lock::lock_cask(cfg, &cask_token)?)
        };
        let dirs = CaskDirs::read_or_resolve(cfg, &installed.config_path(), &[]);
        if let Err(error) = uninstall_installed_cask(cfg, &dirs, &installed, Some(entry), opts) {
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
    opts: CaskUninstallOptions,
) -> Result<()> {
    let specs = install::installed_specs(cfg, dirs, installed, entry);
    let ctx = install::installed_context(installed);
    let artifact_opts = ArtifactOptions {
        force: opts.force,
        dry_run: opts.dry_run,
        ..ArtifactOptions::default()
    };

    output::ohai(&format!(
        "{} Cask {}",
        if opts.dry_run {
            "Would uninstall"
        } else {
            "Uninstalling"
        },
        installed.token
    ));
    if specs.is_empty() && entry.is_none() {
        output::opoo(&format!(
            "No uninstall artifact metadata is available for Cask '{}'.\nHomebrew will remove its records, but files installed by the Cask may remain.",
            installed.token
        ));
    }

    let result =
        super::artifacts::uninstall_specs(cfg, dirs, &specs, &ctx, opts.zap, artifact_opts);
    if result.is_err() && !opts.force {
        return result;
    }

    if opts.dry_run {
        // Nothing above this point touched the disk either: only report the
        // records and staged files a real uninstall would purge.
        if opts.zap {
            output::ohai(&format!(
                "Would remove all staged versions of Cask '{}'",
                installed.token
            ));
        } else {
            output::ohai(&format!(
                "Would purge files for version {} of Cask {}",
                installed.version, installed.token
            ));
        }
        return result;
    }

    let metadata_dir = installed.metadata_main_container_path();
    let _ = std::fs::remove_file(installed.receipt_path());
    let _ = std::fs::remove_file(installed.config_path());
    let _ = std::fs::remove_file(installed.download_sha_path());

    if opts.zap {
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
        if opts.force {
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
