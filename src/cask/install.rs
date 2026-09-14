//! `install --cask`, `reinstall --cask`, `upgrade --cask`, `fetch --cask`.
//!
//! The pipeline follows `docs/DESIGN.md` 8 and `Library/Homebrew/cask/installer.rb`.
//! Token-level entry points resolve names through [`crate::resolve::resolve_cask`]
//! and take the cask lock; all of the work lives in the entry-level functions
//! that take a `&CaskEntry`, so they can be driven directly from tests.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::api::index::Index;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{CaskEntry, sym};
use crate::output;
use crate::platform::{Host, MacOsVersion};

use super::artifacts::{self, ArtifactOptions, ArtifactSpec, CaskContext};
use super::config::CaskDirs;
use super::metadata::{self, ReceiptInput};
use super::{InstalledCask, quarantine, unpack};

#[derive(Debug, Clone, Default)]
pub struct CaskInstallOptions {
    pub force: bool,
    pub adopt: bool,
    pub skip_cask_deps: bool,
    pub dry_run: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub reinstall: bool,
    pub explicit_dir_flags: Vec<String>,
    /// `--no-binaries`.
    pub skip_binaries: bool,
    /// Installed to satisfy another cask or formula (`installed_on_request: false`).
    pub installed_as_dependency: bool,
    /// Reverse the artifacts with `zap` when replacing an installed version.
    pub zap: bool,
    /// Set while `upgrade_cask_entry` drives the install.
    pub upgrade: bool,
}

impl CaskInstallOptions {
    fn artifact_options(&self) -> ArtifactOptions {
        ArtifactOptions {
            force: self.force,
            adopt: self.adopt,
            verbose: self.verbose,
            dry_run: self.dry_run,
            skip_binaries: self.skip_binaries,
        }
    }
}

// ------------------------------------------------------- token entry points

pub fn install_casks(
    cfg: &Config,
    index: &Index,
    tokens: &[String],
    opts: &CaskInstallOptions,
) -> Result<()> {
    let mut failed: Vec<String> = Vec::new();
    for token in tokens {
        let cask = crate::resolve::resolve_cask(cfg, index, token)?;
        let _lock = crate::keg::lock::lock_cask(cfg, &cask.token)?;
        let dirs = CaskDirs::resolve(cfg, &opts.explicit_dir_flags);
        if let Err(error) = install_cask_entry(cfg, Some(index), &dirs, &cask, opts) {
            output::onoe(&format!("{}: {error}", cask.full_token()));
            failed.push(cask.full_token());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Error::user(format!(
            "Failed to install {}.",
            failed.join(", ")
        )))
    }
}

pub fn upgrade_casks(
    cfg: &Config,
    index: &Index,
    tokens: &[String],
    greedy: bool,
    opts: &CaskInstallOptions,
) -> Result<()> {
    let targets: Vec<String> = if tokens.is_empty() {
        super::installed_casks(cfg)
            .into_iter()
            .map(|c| c.token)
            .collect()
    } else {
        tokens.to_vec()
    };

    let mut upgrades: Vec<(CaskEntry, InstalledCask)> = Vec::new();
    for token in &targets {
        let cask = crate::resolve::resolve_cask(cfg, index, token)?;
        let Some(installed) = super::installed_cask(cfg, &cask.token) else {
            if tokens.is_empty() {
                continue;
            }
            return Err(Error::user(format!("Cask '{token}' is not installed.")));
        };
        if !is_outdated(&cask, &installed, greedy) {
            if !opts.quiet {
                output::opoo(&format!(
                    "Not upgrading {}, the latest version is already installed",
                    cask.token
                ));
            }
            continue;
        }
        upgrades.push((cask, installed));
    }

    if upgrades.is_empty() {
        return Ok(());
    }
    output::ohai(&format!(
        "Upgrading {} outdated {}:",
        upgrades.len(),
        if upgrades.len() == 1 {
            "package"
        } else {
            "packages"
        }
    ));
    for (cask, installed) in &upgrades {
        println!(
            "{} {} -> {}",
            cask.full_token(),
            installed.version,
            cask.version.as_deref().unwrap_or("latest")
        );
    }
    if opts.dry_run {
        return Ok(());
    }

    for (cask, installed) in &upgrades {
        let _lock = crate::keg::lock::lock_cask(cfg, &cask.token)?;
        let dirs =
            CaskDirs::read_or_resolve(cfg, &installed.config_path(), &opts.explicit_dir_flags);
        upgrade_cask_entry(cfg, Some(index), &dirs, cask, installed, opts)?;
    }
    Ok(())
}

pub fn fetch_casks(cfg: &Config, index: &Index, tokens: &[String], force: bool) -> Result<()> {
    for token in tokens {
        let cask = crate::resolve::resolve_cask(cfg, index, token)?;
        fetch_cask_entry(cfg, &cask, force)?;
    }
    Ok(())
}

/// Download a cask's container without installing it.
pub fn fetch_cask_entry(cfg: &Config, cask: &CaskEntry, force: bool) -> Result<PathBuf> {
    if force && let Some(url) = cask.url() {
        let guessed = super::download::parse_basename(url, true);
        let cached = super::download::cached_location(cfg, url, &guessed);
        let _ = std::fs::remove_file(cached);
    }
    super::download::download_cask(cfg, cask, false)
}

// -------------------------------------------------------- entry level

/// Install one resolved cask entry.
///
/// `index` is only needed to install formula dependencies; pass `None` when the
/// cask has none (which is what the tests do).
pub fn install_cask_entry(
    cfg: &Config,
    index: Option<&Index>,
    dirs: &CaskDirs,
    cask: &CaskEntry,
    opts: &CaskInstallOptions,
) -> Result<()> {
    prelude(cfg, cask)?;

    let installed = super::installed_cask(cfg, &cask.token);
    if let Some(installed) = &installed
        && !opts.reinstall
        && !opts.force
        && !opts.upgrade
    {
        if cfg.no_install_upgrade {
            output::opoo(&format!("Cask '{}' is already installed.", cask.token));
            return Ok(());
        }
        if !is_outdated(cask, installed, false) {
            if !opts.quiet {
                output::opoo(&format!(
                    "Not upgrading {}, the latest version is already installed",
                    cask.token
                ));
            }
            return Ok(());
        }
        return upgrade_cask_entry(cfg, index, dirs, cask, installed, opts);
    }

    let version = cask.version.clone().unwrap_or_else(|| "latest".to_string());
    if opts.dry_run {
        println!("Would install cask {} {version}", cask.full_token());
        return Ok(());
    }

    let download = super::download::download_cask(cfg, cask, opts.quiet)?;
    install_dependencies(cfg, index, dirs, cask, opts)?;

    if let Some(installed) = &installed {
        // Reinstall/force: remove the existing version first.
        super::uninstall::uninstall_installed_cask(
            cfg,
            dirs,
            installed,
            Some(cask),
            opts.zap,
            true,
        )?;
    }

    output::ohai(&format!("Installing Cask {}", cask.token));
    let ctx = CaskContext::from_entry(cfg, cask);
    let specs = artifacts::artifact_specs(cfg, dirs, cask);
    let uninstall_artifacts = artifacts::specs_to_json(&specs);

    stage(cfg, cask, &download, &ctx.staged_path, opts.verbose)?;

    let input = ReceiptInput {
        cask,
        metadata_subdir: ctx
            .caskroom_path
            .join(super::METADATA_SUBDIR)
            .join(&version)
            .join(super::new_timestamp()),
        receipt_path: ctx
            .caskroom_path
            .join(super::METADATA_SUBDIR)
            .join("INSTALL_RECEIPT.json"),
        config_path: ctx
            .caskroom_path
            .join(super::METADATA_SUBDIR)
            .join("config.json"),
        uninstall_artifacts,
        installed_on_request: !opts.installed_as_dependency,
        tap_git_head: index.map(|i| i.metadata().cask_tap_git_head.clone()),
        api_path: Some(api_file_path(cfg)),
        runtime_dependencies: runtime_dependencies(cask),
    };
    metadata::write_caskfile(&input, None)?;

    if let Err(error) = artifacts::install_specs(cfg, dirs, &specs, &ctx, opts.artifact_options()) {
        purge_versioned_files(cfg, cask, &ctx, opts.upgrade);
        return Err(error);
    }

    metadata::write_config(cfg, dirs, &input.config_path, &opts.explicit_dir_flags)?;
    metadata::write_receipt(&input)?;
    if cask.is_latest() {
        let _ = crate::keg::atomic_write(
            &ctx.caskroom_path
                .join(super::METADATA_SUBDIR)
                .join("LATEST_DOWNLOAD_SHA256"),
            super::download::file_sha256(&download)?.as_bytes(),
        );
    }

    if let Some(caveats) = caveats(cfg, dirs, cask) {
        output::ohai("Caveats");
        println!("{caveats}");
    }
    println!("{}", summary(cfg, cask, opts.upgrade));
    Ok(())
}

/// Install the new version, then purge the predecessor
/// (`Cask::Upgrade.upgrade_cask`).
pub fn upgrade_cask_entry(
    cfg: &Config,
    index: Option<&Index>,
    dirs: &CaskDirs,
    cask: &CaskEntry,
    installed: &InstalledCask,
    opts: &CaskInstallOptions,
) -> Result<()> {
    output::ohai(&format!("Upgrading {}", cask.token));
    println!(
        "  {} -> {}",
        installed.version,
        cask.version.as_deref().unwrap_or("latest")
    );

    let mut opts = opts.clone();
    opts.upgrade = true;
    opts.reinstall = false;

    // The predecessor's artifacts move back into its staged directory, then the
    // new version installs over them.
    let predecessor_specs = installed_specs(cfg, dirs, installed, Some(cask));
    let predecessor_ctx = installed_context(installed);
    artifacts::uninstall_specs(
        cfg,
        dirs,
        &predecessor_specs,
        &predecessor_ctx,
        false,
        ArtifactOptions {
            force: true,
            ..opts.artifact_options()
        },
    )?;

    install_cask_entry(cfg, index, dirs, cask, &opts)?;

    if installed.version != cask.version.as_deref().unwrap_or("latest") {
        output::ohai(&format!(
            "Purging files for version {} of Cask {}",
            installed.version, cask.token
        ));
        let _ = unpack::remove_path(&installed.staged_path());
        let _ = std::fs::remove_dir_all(installed.metadata_versioned_path());
    }
    Ok(())
}

// ------------------------------------------------------------- checks

/// `Cask::Installer#prelude`.
pub fn prelude(cfg: &Config, cask: &CaskEntry) -> Result<()> {
    check_deprecate_disable(cask)?;
    check_conflicts(cfg, cask)?;
    check_requirements(cask)
}

/// `Cask::Installer#check_deprecate_disable`.
pub fn check_deprecate_disable(cask: &CaskEntry) -> Result<()> {
    if let Some(args) = &cask.disable_args {
        return Err(Error::user(format!(
            "{} has been disabled{}",
            cask.token,
            because(args)
        )));
    }
    if let Some(args) = &cask.deprecate_args {
        output::opoo(&format!(
            "{} has been deprecated{}",
            cask.token,
            because(args)
        ));
    }
    Ok(())
}

fn because(args: &Value) -> String {
    let reason = args
        .get(":because")
        .and_then(Value::as_str)
        .map(|r| sym(r).replace('_', " "));
    let date = args.get(":date").and_then(Value::as_str);
    match (reason, date) {
        (Some(reason), Some(date)) => {
            format!(" because it {reason}! It will be disabled on {date}.")
        }
        (Some(reason), None) => format!(" because it {reason}!"),
        (None, Some(date)) => format!("! It will be disabled on {date}."),
        (None, None) => "!".to_string(),
    }
}

/// `Cask::Installer#check_conflicts`.
pub fn check_conflicts(cfg: &Config, cask: &CaskEntry) -> Result<()> {
    let Some(conflicts) = &cask.conflicts_with_args else {
        return Ok(());
    };
    let list = |key: &str| -> Vec<String> {
        match conflicts.get(key) {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => vec![],
        }
    };
    for token in list(":cask") {
        if super::installed_cask(cfg, &token).is_some() {
            return Err(Error::user(format!(
                "Cask '{}' conflicts with '{token}'.",
                cask.token
            )));
        }
    }
    for name in list(":formula") {
        if !crate::keg::installed_kegs(cfg, &name).is_empty() {
            return Err(Error::user(format!(
                "Cask '{}' conflicts with '{name}'.",
                cask.token
            )));
        }
    }
    Ok(())
}

/// `Cask::Installer#check_requirements`: macOS version and architecture.
pub fn check_requirements(cask: &CaskEntry) -> Result<()> {
    let host = Host::detect();
    if let Some(requirement) = cask.macos_requirement()
        && let Some(current) = host.macos
        && let Some(message) = macos_requirement_error(requirement, ">=", current)
    {
        return Err(Error::user(format!("{}: {message}", cask.token)));
    }
    if let Some(requirement) = cask
        .depends_on_args
        .as_ref()
        .and_then(|d| d.get(":maximum_macos"))
        && let Some(current) = host.macos
        && let Some(message) = macos_requirement_error(requirement, "<=", current)
    {
        return Err(Error::user(format!("{}: {message}", cask.token)));
    }

    if let Some(arch) = cask.arch_requirement() {
        let wanted: Vec<String> = match arch {
            Value::String(s) => vec![sym(s).to_string()],
            Value::Array(a) => a
                .iter()
                .filter_map(Value::as_str)
                .map(|s| sym(s).to_string())
                .collect(),
            _ => vec![],
        };
        let current = match host.arch {
            crate::platform::Arch::Arm64 => "arm64",
            crate::platform::Arch::X86_64 => "intel",
        };
        if !wanted.is_empty()
            && !wanted
                .iter()
                .any(|a| a == current || a == host.arch.as_str())
        {
            return Err(Error::user(format!(
                "{}: This cask depends on hardware architecture being one of [{}], but you are running {current}.",
                cask.token,
                wanted
                    .iter()
                    .map(|a| format!(":{a}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(())
}

/// `MacOSRequirement#allows?` plus the cask wording of `#message`.
///
/// A bare symbol is a `>=` requirement, an array is `==` against any of them,
/// and `{"<=": [":sonoma"]}` carries its own comparator.
fn macos_requirement_error(
    requirement: &Value,
    default_comparator: &str,
    current: MacOsVersion,
) -> Option<String> {
    let (comparator, symbols) = parse_macos_requirement(requirement, default_comparator)?;
    let versions: Vec<u32> = symbols
        .iter()
        .filter_map(|s| MacOsVersion::major_for_symbol(s))
        .collect();
    if versions.is_empty() {
        return None;
    }
    let allowed = match comparator.as_str() {
        ">=" => versions.iter().any(|v| current.major >= *v),
        "<=" => versions.iter().any(|v| current.major <= *v),
        ">" => versions.iter().any(|v| current.major > *v),
        "<" => versions.iter().any(|v| current.major < *v),
        _ => versions.contains(&current.major),
    };
    if allowed {
        return None;
    }
    let names: Vec<String> = symbols.iter().map(|s| pretty_name(s)).collect();
    Some(match comparator.as_str() {
        ">=" => format!(
            "This cask does not run on macOS versions older than {}.",
            names.join(", ")
        ),
        "<=" => format!(
            "This cask does not run on macOS versions newer than {}.",
            names.join(", ")
        ),
        _ if names.len() > 1 => {
            let (last, rest) = names.split_last().expect("non-empty");
            format!(
                "This cask does not run on macOS versions other than {} and {last}.",
                rest.join(", ")
            )
        }
        _ => format!(
            "This cask does not run on macOS versions other than {}.",
            names.join(", ")
        ),
    })
}

fn parse_macos_requirement(
    requirement: &Value,
    default_comparator: &str,
) -> Option<(String, Vec<String>)> {
    match requirement {
        Value::String(s) => {
            let symbol = sym(s);
            // `:any` means no requirement at all.
            (symbol != "any").then(|| (default_comparator.to_string(), vec![symbol.to_string()]))
        }
        Value::Array(items) => {
            let symbols: Vec<String> = items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| sym(s).to_string())
                .collect();
            (!symbols.is_empty()).then_some(("==".to_string(), symbols))
        }
        Value::Object(map) => map.iter().next().map(|(comparator, value)| {
            let symbols = match value {
                Value::Array(a) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|s| sym(s).to_string())
                    .collect(),
                Value::String(s) => vec![sym(s).to_string()],
                _ => vec![],
            };
            (comparator.clone(), symbols)
        }),
        _ => None,
    }
}

/// `MacOSVersion#pretty_name`: `:big_sur` -> `Big Sur`.
pub fn pretty_name(symbol: &str) -> String {
    symbol
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ------------------------------------------------------- dependencies

fn install_dependencies(
    cfg: &Config,
    index: Option<&Index>,
    dirs: &CaskDirs,
    cask: &CaskEntry,
    opts: &CaskInstallOptions,
) -> Result<()> {
    let formulae: Vec<String> = cask
        .formula_dependencies()
        .into_iter()
        .filter(|name| crate::keg::installed_kegs(cfg, name).is_empty())
        .collect();
    let casks: Vec<String> = cask
        .cask_dependencies()
        .into_iter()
        .filter(|token| super::installed_cask(cfg, token).is_none())
        .collect();
    if formulae.is_empty() && casks.is_empty() {
        return Ok(());
    }

    let mut all = casks.clone();
    all.extend(formulae.clone());
    output::ohai(&format!("Installing dependencies: {}", all.join(", ")));

    if !casks.is_empty() {
        if opts.skip_cask_deps {
            for token in &casks {
                output::opoo(&format!(
                    "`--skip-cask-deps` is set; skipping installation of {token}."
                ));
            }
        } else {
            let Some(index) = index else {
                return Err(Error::user(format!(
                    "Cask '{}' depends on the casks {} but no package index is available.",
                    cask.token,
                    casks.join(", ")
                )));
            };
            for token in &casks {
                let dependency = crate::resolve::resolve_cask(cfg, index, token)?;
                let mut dep_opts = opts.clone();
                dep_opts.installed_as_dependency = true;
                dep_opts.reinstall = false;
                dep_opts.upgrade = false;
                install_cask_entry(cfg, Some(index), dirs, &dependency, &dep_opts)?;
            }
        }
    }

    if !formulae.is_empty() {
        let Some(index) = index else {
            return Err(Error::user(format!(
                "Cask '{}' depends on the formulae {} but no package index is available.",
                cask.token,
                formulae.join(", ")
            )));
        };
        let formula_opts = crate::ops::install::InstallOptions {
            quiet: opts.quiet,
            verbose: opts.verbose,
            on_request: false,
            ..Default::default()
        };
        crate::ops::install::install_formulae(cfg, index, &formulae, &formula_opts)?;
    }
    Ok(())
}

/// `Cask::Tab.runtime_deps_hash`.
fn runtime_dependencies(cask: &CaskEntry) -> Value {
    let mut map = serde_json::Map::new();
    let casks = cask.cask_dependencies();
    if !casks.is_empty() {
        map.insert(
            "cask".into(),
            Value::Array(
                casks
                    .into_iter()
                    .map(|token| serde_json::json!({"full_name": token, "declared_directly": true}))
                    .collect(),
            ),
        );
    }
    let formulae = cask.formula_dependencies();
    if !formulae.is_empty() {
        map.insert(
            "formula".into(),
            Value::Array(
                formulae
                    .into_iter()
                    .map(|name| serde_json::json!({"full_name": name, "declared_directly": true}))
                    .collect(),
            ),
        );
    }
    Value::Object(map)
}

// ------------------------------------------------------------ staging

/// `Cask::Installer#stage`: unpack the download into the Caskroom and carry the
/// quarantine attribute over.
fn stage(
    cfg: &Config,
    cask: &CaskEntry,
    download: &Path,
    staged: &Path,
    verbose: bool,
) -> Result<()> {
    if let Some(parent) = staged.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if staged.exists() || staged.is_symlink() {
        unpack::remove_path(staged)?;
    }
    unpack::unpack_container_args(cfg, download, cask.container_args.as_ref(), staged, verbose)?;
    quarantine::propagate(download, staged)
}

/// `Cask::Installer#purge_versioned_files` for a failed install.
fn purge_versioned_files(cfg: &Config, cask: &CaskEntry, ctx: &CaskContext, upgrade: bool) {
    output::ohai(&format!(
        "Purging files for version {} of Cask {}",
        ctx.version, cask.token
    ));
    let _ = unpack::remove_path(&ctx.staged_path);
    let versioned = ctx
        .caskroom_path
        .join(super::METADATA_SUBDIR)
        .join(&ctx.version);
    let _ = std::fs::remove_dir_all(&versioned);
    if !upgrade {
        let _ = std::fs::remove_dir(ctx.caskroom_path.join(super::METADATA_SUBDIR));
        let _ = std::fs::remove_dir(&ctx.caskroom_path);
    }
    let _ = cfg;
}

/// `Cask::Installer#summary`.
pub fn summary(cfg: &Config, cask: &CaskEntry, upgrade: bool) -> String {
    let verb = if upgrade { "upgraded" } else { "installed" };
    if cfg.no_emoji {
        format!("{} was successfully {verb}!", cask.token)
    } else {
        format!(
            "{}  {} was successfully {verb}!",
            cfg.install_badge, cask.token
        )
    }
}

/// Caveats with the API placeholders expanded.
pub fn caveats(cfg: &Config, dirs: &CaskDirs, cask: &CaskEntry) -> Option<String> {
    let mut text = cask.caveats_text().map(|t| {
        cfg.expand_placeholders(&t)
            .replace("$APPDIR", &dirs.appdir.to_string_lossy())
    });
    if cask.caveats_rosetta && Host::detect().arch == crate::platform::Arch::Arm64 {
        let rosetta = format!(
            "{} requires Rosetta 2 to be installed because it is an Intel-only cask.\nTo install Rosetta 2, run:\n  softwareupdate --install-rosetta --agree-to-license",
            cask.token
        );
        text = Some(match text {
            Some(existing) => format!("{rosetta}\n{existing}"),
            None => rosetta,
        });
    }
    text
}

/// Path recorded in the receipt's `source.path`.
pub fn api_file_path(cfg: &Config) -> String {
    let tag = Host::detect().bottle_tag();
    cfg.cache_api()
        .join(format!("internal/packages.{tag}.jws.json"))
        .to_string_lossy()
        .into_owned()
}

/// `Cask#outdated_version`, restricted to what the internal API can answer.
pub fn is_outdated(cask: &CaskEntry, installed: &InstalledCask, greedy: bool) -> bool {
    match cask.version.as_deref() {
        None => false,
        Some("latest") => greedy,
        Some(version) => version != installed.version,
    }
}

/// Artifacts of an installed cask, preferring the recorded ones so an uninstall
/// removes exactly what the install created.
pub fn installed_specs(
    cfg: &Config,
    dirs: &CaskDirs,
    installed: &InstalledCask,
    entry: Option<&CaskEntry>,
) -> Vec<ArtifactSpec> {
    let recorded = metadata::receipt_uninstall_artifacts(&installed.receipt_path());
    let recorded = match installed
        .caskfile_path()
        .and_then(|p| metadata::caskfile_artifacts(&p))
    {
        // The installed caskfile's `artifacts` key overrides the receipt.
        Some(list) => list,
        None => recorded,
    };
    if !recorded.is_empty() {
        return artifacts::artifact_specs_from_receipt(cfg, dirs, &recorded);
    }
    match entry {
        Some(entry) => artifacts::artifact_specs(cfg, dirs, entry),
        None => vec![],
    }
}

/// Artifact context for an already-installed cask.
pub fn installed_context(installed: &InstalledCask) -> CaskContext {
    CaskContext {
        token: installed.token.clone(),
        name: installed.token.clone(),
        version: installed.version.clone(),
        staged_path: installed.staged_path(),
        caskroom_path: installed.caskroom_path.clone(),
        only_path: installed
            .caskfile_path()
            .and_then(|p| metadata::caskfile_only_path(&p)),
        auto_updates: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cask(json: Value) -> CaskEntry {
        let mut cask: CaskEntry = serde_json::from_value(json).unwrap();
        cask.token = "demo".into();
        cask
    }

    #[test]
    fn macos_requirements() {
        let ventura = MacOsVersion::parse("13.0").unwrap();
        let tahoe = MacOsVersion::parse("26.0").unwrap();

        // `:any` never fails.
        assert!(macos_requirement_error(&serde_json::json!(":any"), ">=", ventura).is_none());
        // A bare symbol is a minimum.
        assert!(macos_requirement_error(&serde_json::json!(":ventura"), ">=", tahoe).is_none());
        let message =
            macos_requirement_error(&serde_json::json!(":sequoia"), ">=", ventura).unwrap();
        assert_eq!(
            message,
            "This cask does not run on macOS versions older than Sequoia."
        );
        // An array is an exact set.
        assert!(
            macos_requirement_error(&serde_json::json!([":ventura", ":sonoma"]), ">=", ventura)
                .is_none()
        );
        let message =
            macos_requirement_error(&serde_json::json!([":ventura", ":sonoma"]), ">=", tahoe)
                .unwrap();
        assert_eq!(
            message,
            "This cask does not run on macOS versions other than Ventura and Sonoma."
        );
        // The comparator hash form.
        assert!(
            macos_requirement_error(&serde_json::json!({">=": [":ventura"]}), ">=", tahoe)
                .is_none()
        );
        let message = macos_requirement_error(&serde_json::json!(":sonoma"), "<=", tahoe).unwrap();
        assert_eq!(
            message,
            "This cask does not run on macOS versions newer than Sonoma."
        );
    }

    #[test]
    fn pretty_names() {
        assert_eq!(pretty_name("big_sur"), "Big Sur");
        assert_eq!(pretty_name("ventura"), "Ventura");
        assert_eq!(pretty_name("golden_gate"), "Golden Gate");
    }

    #[test]
    fn deprecation_and_disabling() {
        let deprecated = cask(serde_json::json!({
            "deprecate_args": {":date": "2025-01-01", ":because": ":discontinued"}
        }));
        assert!(check_deprecate_disable(&deprecated).is_ok());

        let disabled = cask(serde_json::json!({
            "disable_args": {":date": "2025-01-01", ":because": ":discontinued"}
        }));
        let error = check_deprecate_disable(&disabled).unwrap_err().to_string();
        assert_eq!(
            error,
            "demo has been disabled because it discontinued! It will be disabled on 2025-01-01."
        );
    }

    #[test]
    fn outdated_rules() {
        let installed = InstalledCask {
            token: "demo".into(),
            version: "1.0".into(),
            caskroom_path: PathBuf::from("/tmp/demo"),
            metadata_path: None,
        };
        assert!(is_outdated(
            &cask(serde_json::json!({"version": "1.1"})),
            &installed,
            false
        ));
        assert!(!is_outdated(
            &cask(serde_json::json!({"version": "1.0"})),
            &installed,
            false
        ));
        // `version :latest` only counts as outdated when greedy.
        assert!(!is_outdated(
            &cask(serde_json::json!({"version": "latest"})),
            &installed,
            false
        ));
        assert!(is_outdated(
            &cask(serde_json::json!({"version": "latest"})),
            &installed,
            true
        ));
    }

    #[test]
    fn summary_line() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = crate::cask::tests_support::config(tmp.path());
        let cask = cask(serde_json::json!({"version": "1.0"}));
        assert_eq!(
            summary(&cfg, &cask, false),
            "🍺  demo was successfully installed!"
        );
        assert_eq!(
            summary(&cfg, &cask, true),
            "🍺  demo was successfully upgraded!"
        );
        cfg.no_emoji = true;
        assert_eq!(
            summary(&cfg, &cask, false),
            "demo was successfully installed!"
        );
    }

    #[test]
    fn conflicts_with_installed_cask() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::cask::tests_support::config(tmp.path());
        let conflicting = cask(serde_json::json!({
            "conflicts_with_args": {":cask": ["other"]}
        }));
        assert!(check_conflicts(&cfg, &conflicting).is_ok());

        let meta = cfg
            .caskroom()
            .join("other/.metadata/1.0/20250101000000.000/Casks");
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(meta.join("other.json"), "{}").unwrap();
        let error = check_conflicts(&cfg, &conflicting).unwrap_err().to_string();
        assert_eq!(error, "Cask 'demo' conflicts with 'other'.");
    }
}
