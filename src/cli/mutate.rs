//! Mutating commands: parse Homebrew's flags and hand the work to `ops`,
//! `cask`, `services` and `tap`.
//!
//! The heavy lifting lives in those modules; this file only builds their
//! option structs, resolves names and applies the delegation rules from
//! `docs/DESIGN.md` 5 (source builds and `--HEAD` go to the Ruby `brew`).

use clap::Args;

use crate::cask::install::CaskInstallOptions;
use crate::error::{Error, Result};
use crate::ops::cleanup::CleanupOptions;
use crate::ops::install::InstallOptions;
use crate::ops::uninstall::UninstallOptions;
use crate::ops::upgrade::UpgradeOptions;
use crate::output;
use crate::resolve::{self, Kind};

use super::Ctx;

fn kind_of(formula: bool, cask: bool) -> Kind {
    match (formula, cask) {
        (true, false) => Kind::Formula,
        (false, true) => Kind::Cask,
        _ => Kind::Any,
    }
}

/// Split names into resolved formulae and casks.
///
/// The resolved entry is what reaches `ops` and `cask`, not its bare name:
/// re-resolving `user/repo/jq` downstream would give core's `jq`, because the
/// API loader runs before any tap loader, and the same holds for a cask a tap
/// qualifies.
///
/// `acting_on_installed` names the command for
/// [`resolve::check_installed_tap`], which refuses a tap-qualified name whose
/// rack holds another tap's package. `fetch` passes `None`: it only downloads,
/// and never touches the rack.
fn partition(
    ctx: &Ctx,
    names: &[String],
    kind: Kind,
    acting_on_installed: Option<&str>,
) -> Result<(
    Vec<crate::model::FormulaEntry>,
    Vec<crate::model::CaskEntry>,
)> {
    let index = ctx.index()?;
    let mut formulae = Vec::new();
    let mut casks = Vec::new();
    for name in names {
        match resolve::resolve(&ctx.cfg, index, name, kind)? {
            resolve::Resolved::Formula(f) => {
                if let Some(action) = acting_on_installed {
                    resolve::check_installed_tap(&ctx.cfg, name, &f, action)?;
                }
                formulae.push(f);
            }
            resolve::Resolved::Cask(c) => casks.push(c),
        }
    }
    Ok((formulae, casks))
}

#[derive(Args, Debug)]
pub struct InstallArgs {
    #[arg(value_name = "formula|cask", required = true)]
    pub names: Vec<String>,
    // Homebrew: `conflicts "--formula", "--cask"`.
    #[arg(long, visible_alias = "formulae", conflicts_with = "cask")]
    pub formula: bool,
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
    /// Install the dependencies but not the formula itself.
    #[arg(long)]
    pub only_dependencies: bool,
    /// Skip installing any dependencies.
    #[arg(long)]
    pub ignore_dependencies: bool,
    #[arg(short = 'f', long)]
    pub force: bool,
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    /// Delete files that already exist in the prefix while linking.
    #[arg(long)]
    pub overwrite: bool,
    #[arg(long)]
    pub skip_post_install: bool,
    /// Compile from source instead of pouring a bottle (delegates to brew).
    #[arg(short = 's', long)]
    pub build_from_source: bool,
    /// Install the HEAD version (delegates to brew).
    #[arg(long = "HEAD")]
    pub head: bool,
    #[arg(long)]
    pub keep_tmp: bool,
    /// Adopt an existing app at the target path (casks).
    #[arg(long)]
    pub adopt: bool,
    #[arg(long)]
    pub skip_cask_deps: bool,
    /// Require all casks to have a checksum.
    #[arg(long)]
    pub require_sha: bool,
    /// Disable linking of a cask's helper executables.
    #[arg(long)]
    pub no_binaries: bool,
    /// Enable linking of a cask's helper executables (the default).
    #[arg(long, conflicts_with = "no_binaries")]
    pub binaries: bool,
    /// Do not quarantine a cask's download and staged files.
    #[arg(long)]
    pub no_quarantine: bool,
    /// Quarantine a cask's download and staged files (the default).
    #[arg(long, conflicts_with = "no_quarantine")]
    pub quarantine: bool,
    #[arg(long, value_name = "path")]
    pub appdir: Option<String>,
    #[arg(long, value_name = "path")]
    pub fontdir: Option<String>,
}

pub fn install(ctx: &Ctx, args: &InstallArgs, reinstall: bool) -> Result<()> {
    if args.build_from_source || args.head {
        let reason = if args.head {
            "`--HEAD` installs need the Ruby formula DSL"
        } else {
            "`--build-from-source` needs the Ruby formula DSL"
        };
        return ctx.delegate(reason);
    }
    crate::update::auto_update_if_needed(&ctx.cfg, "install", &args.names);
    let action = if reinstall { "reinstall" } else { "install" };
    let (formulae, casks) = partition(
        ctx,
        &args.names,
        kind_of(args.formula, args.cask),
        Some(action),
    )?;
    let index = ctx.index()?;

    if !formulae.is_empty() {
        let opts = InstallOptions {
            only_dependencies: args.only_dependencies,
            ignore_dependencies: args.ignore_dependencies,
            force: args.force,
            dry_run: args.dry_run,
            overwrite: args.overwrite,
            skip_post_install: args.skip_post_install,
            quiet: ctx.quiet,
            verbose: ctx.verbose,
            reinstall,
            on_request: true,
            build_from_source: args.build_from_source,
            head: args.head,
            keep_tmp: args.keep_tmp,
        };
        crate::ops::install::install_formulae_entries(&ctx.cfg, index, &formulae, &opts)?;
    }
    if !casks.is_empty() {
        let opts = cask_options(ctx, args, reinstall);
        crate::cask::install::install_casks(&ctx.cfg, index, &casks, &opts)?;
    }
    Ok(())
}

fn cask_options(ctx: &Ctx, args: &InstallArgs, reinstall: bool) -> CaskInstallOptions {
    let mut explicit_dir_flags = Vec::new();
    if let Some(d) = &args.appdir {
        explicit_dir_flags.push(format!("--appdir={d}"));
    }
    if let Some(d) = &args.fontdir {
        explicit_dir_flags.push(format!("--fontdir={d}"));
    }
    // `cask_options`: the switches come from the command line first, then
    // from `HOMEBREW_CASK_OPTS`.
    let env = &ctx.cfg.cask_opts;
    let binaries = if args.binaries {
        Some(true)
    } else if args.no_binaries {
        Some(false)
    } else {
        crate::cask::config::bool_flag(env, "binaries")
    };
    let quarantine = if args.quarantine {
        Some(true)
    } else if args.no_quarantine {
        Some(false)
    } else {
        crate::cask::config::bool_flag(env, "quarantine")
    };
    CaskInstallOptions {
        force: args.force,
        adopt: args.adopt,
        skip_cask_deps: args.skip_cask_deps,
        dry_run: args.dry_run,
        quiet: ctx.quiet,
        verbose: ctx.verbose,
        reinstall,
        explicit_dir_flags,
        skip_binaries: binaries == Some(false),
        require_sha: args.require_sha
            || crate::cask::config::bool_flag(env, "require-sha").unwrap_or(false),
        installed_as_dependency: false,
        zap: false,
        upgrade: false,
        dependency_chain: Vec::new(),
        no_quarantine: quarantine == Some(false),
    }
}

#[derive(Args, Debug)]
pub struct UpgradeArgs {
    #[arg(value_name = "formula|cask")]
    pub names: Vec<String>,
    // Homebrew: `conflicts "--formula", "--cask"`.
    #[arg(long, visible_alias = "formulae", conflicts_with = "cask")]
    pub formula: bool,
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    #[arg(short = 'f', long)]
    pub force: bool,
    #[arg(short = 'g', long)]
    pub greedy: bool,
    /// Also upgrade casks with `version :latest`.
    #[arg(long)]
    pub greedy_latest: bool,
    /// Also upgrade casks with `auto_updates true`.
    #[arg(long)]
    pub greedy_auto_updates: bool,
    /// Do not quarantine a cask's download and staged files.
    #[arg(long)]
    pub no_quarantine: bool,
    /// Quarantine a cask's download and staged files (the default).
    #[arg(long, conflicts_with = "no_quarantine")]
    pub quarantine: bool,
}

pub fn upgrade(ctx: &Ctx, args: &UpgradeArgs) -> Result<()> {
    crate::update::auto_update_if_needed(&ctx.cfg, "upgrade", &args.names);
    let index = ctx.index()?;
    let (formulae, casks) = partition(
        ctx,
        &args.names,
        kind_of(args.formula, args.cask),
        Some("upgrade"),
    )?;
    let want_formulae = !args.cask || args.formula;
    let want_casks = !args.formula || args.cask;

    if want_formulae && (args.names.is_empty() || !formulae.is_empty()) {
        let opts = UpgradeOptions {
            dry_run: args.dry_run,
            force: args.force,
            quiet: ctx.quiet,
            verbose: ctx.verbose,
            greedy: args.greedy,
        };
        crate::ops::upgrade::upgrade_formulae(&ctx.cfg, index, &formulae, &opts)?;
    }
    if want_casks && (args.names.is_empty() || !casks.is_empty()) {
        let quarantine = if args.quarantine {
            Some(true)
        } else if args.no_quarantine {
            Some(false)
        } else {
            crate::cask::config::bool_flag(&ctx.cfg.cask_opts, "quarantine")
        };
        let opts = CaskInstallOptions {
            force: args.force,
            dry_run: args.dry_run,
            quiet: ctx.quiet,
            verbose: ctx.verbose,
            no_quarantine: quarantine == Some(false),
            ..Default::default()
        };
        let greedy = crate::cask::install::Greedy {
            all: args.greedy,
            latest: args.greedy_latest,
            auto_updates: args.greedy_auto_updates,
        };
        crate::cask::install::upgrade_casks(&ctx.cfg, index, &casks, greedy, &opts)?;
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct UninstallArgs {
    #[arg(value_name = "formula|cask", required = true)]
    pub names: Vec<String>,
    // Homebrew: `conflicts "--formula", "--cask"`.
    #[arg(long, visible_alias = "formulae", conflicts_with = "cask")]
    pub formula: bool,
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
    #[arg(short = 'f', long)]
    pub force: bool,
    #[arg(long)]
    pub ignore_dependencies: bool,
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    /// Also remove all files associated with a cask.
    #[arg(long)]
    pub zap: bool,
}

pub fn uninstall(ctx: &Ctx, args: &UninstallArgs) -> Result<()> {
    let index = ctx.index()?;
    let (formulae, casks) = partition(
        ctx,
        &args.names,
        kind_of(args.formula, args.cask),
        Some("uninstall"),
    )?;
    if !formulae.is_empty() {
        let opts = UninstallOptions {
            force: args.force,
            ignore_dependencies: args.ignore_dependencies,
            dry_run: args.dry_run,
        };
        crate::ops::uninstall::uninstall_formulae(&ctx.cfg, index, &formulae, &opts)?;
    }
    if !casks.is_empty() {
        let opts = crate::cask::uninstall::CaskUninstallOptions {
            zap: args.zap,
            force: args.force,
            dry_run: args.dry_run,
        };
        crate::cask::uninstall::uninstall_casks(&ctx.cfg, &casks, opts)?;
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct AutoremoveArgs {
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

pub fn autoremove(ctx: &Ctx, args: &AutoremoveArgs) -> Result<()> {
    let index = ctx.index()?;
    crate::ops::uninstall::autoremove(&ctx.cfg, index, args.dry_run)
}

#[derive(Args, Debug)]
pub struct CleanupArgs {
    #[arg(value_name = "formula|cask")]
    pub names: Vec<String>,
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    /// Scrub the cache, removing all downloads.
    #[arg(short = 's')]
    pub scrub: bool,
    /// Remove all cache files older than this many days.
    #[arg(long, value_name = "days")]
    pub prune: Option<String>,
    /// Only prune the symlinks and directories from the prefix.
    #[arg(long)]
    pub prune_prefix: bool,
}

pub fn cleanup(ctx: &Ctx, args: &CleanupArgs) -> Result<()> {
    let index = ctx.index()?;
    let prune_days = match args.prune.as_deref() {
        None => None,
        Some("all") => Some(0),
        Some(days) => Some(
            days.parse::<u64>()
                .map_err(|_| Error::user(format!("Invalid `--prune` value: {days}")))?,
        ),
    };
    let opts = CleanupOptions {
        dry_run: args.dry_run,
        scrub: args.scrub,
        prune_days,
        prune_prefix: args.prune_prefix,
    };
    crate::ops::cleanup::cleanup(&ctx.cfg, index, &args.names, &opts)
}

#[derive(Args, Debug)]
pub struct LinkArgs {
    #[arg(value_name = "formula", required = true)]
    pub names: Vec<String>,
    /// Delete files that already exist in the prefix while linking.
    #[arg(long)]
    pub overwrite: bool,
    /// Allow keg-only formulae to be linked.
    #[arg(short = 'f', long)]
    pub force: bool,
    /// List the files that would be linked or deleted.
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    // Homebrew: `conflicts "--formula", "--cask"`.
    #[arg(long, visible_alias = "formulae", conflicts_with = "cask")]
    pub formula: bool,
    /// Link a cask's binaries, manpages and completions (delegates to brew).
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
}

/// The keg `link`/`unlink`/`postinstall` operate on: the latest installed
/// version (`NamedArgs#resolve_latest_keg`), or `NoSuchKegError`.
fn latest_keg(ctx: &Ctx, name: &str) -> Result<crate::keg::Keg> {
    crate::keg::latest_keg(&ctx.cfg, name).ok_or_else(|| {
        // `NoSuchKegError#to_s`.
        Error::user(format!("No such keg: {}", ctx.cfg.rack(name).display()))
    })
}

pub fn link(ctx: &Ctx, args: &LinkArgs, link_it: bool) -> Result<()> {
    if args.cask {
        let what = if link_it { "link" } else { "unlink" };
        return ctx.delegate(&format!("`{what} --cask` is not implemented by fastbrew"));
    }
    let index = ctx.index()?;
    let opts = crate::keg::link::LinkOptions {
        overwrite: args.overwrite,
        dry_run: args.dry_run,
        verbose: ctx.verbose,
    };
    for name in &args.names {
        // A keg may outlive its formula; fall back to the rack's name. The
        // rack's own receipt picks the tap (`Formulary.from_rack`).
        let formula = resolve::resolve_installed(&ctx.cfg, index, name).ok();
        if let Some(f) = formula.as_ref() {
            let action = if link_it { "link" } else { "unlink" };
            resolve::check_installed_tap(&ctx.cfg, name, f, action)?;
        }
        let short = formula
            .as_ref()
            .map(|f| f.name.clone())
            .unwrap_or_else(|| name.rsplit('/').next().unwrap_or(name).to_string());
        // `link` takes the newest keg (`resolve_latest_keg`), `unlink` the one
        // whose records are actually in the prefix (`resolve_default_keg`).
        let keg = if link_it {
            latest_keg(ctx, &short)?
        } else {
            crate::keg::default_keg(&ctx.cfg, &short).ok_or_else(|| {
                Error::user(format!("No such keg: {}", ctx.cfg.rack(&short).display()))
            })?
        };
        // Linking and unlinking rewrite the prefix records of this rack, so
        // they take the same lock every other destructive operation does. A
        // `--dry-run` only lists, so it stays lock-free.
        let _lock = if args.dry_run {
            None
        } else {
            Some(crate::keg::lock::lock_formula(&ctx.cfg, &short)?)
        };

        if !link_it {
            if args.dry_run {
                println!("Would remove:");
            }
            if !args.dry_run {
                print!("Unlinking {}... ", keg.path.display());
                if ctx.verbose {
                    println!();
                }
                output::flush();
            }
            let removed = crate::keg::link::unlink(&ctx.cfg, &keg, opts)?;
            if !args.dry_run {
                println!("{removed} symlinks removed.");
            }
            continue;
        }

        if keg.is_linked(&ctx.cfg) {
            output::opoo(&format!("Already linked: {}", keg.path.display()));
            let keg_only = formula.as_ref().is_some_and(|f| f.is_keg_only());
            let flag = if keg_only && !is_versioned_keg_only(formula.as_ref()) {
                "--force "
            } else {
                ""
            };
            println!("To relink, run:\n  brew unlink {short} && brew link {flag}{short}");
            continue;
        }

        if args.dry_run {
            println!(
                "{}",
                if args.overwrite {
                    "Would remove:"
                } else {
                    "Would link:"
                }
            );
            let globs = formula
                .as_ref()
                .map(|f| f.link_overwrite_paths.clone())
                .unwrap_or_default();
            crate::keg::link::link(&ctx.cfg, &keg, &globs, opts)?;
            continue;
        }

        if let Some(f) = formula.as_ref().filter(|f| f.is_keg_only()) {
            let by_macos = matches!(
                f.keg_only().map(|(reason, _)| reason),
                Some(crate::model::KegOnly::ProvidedByMacos)
                    | Some(crate::model::KegOnly::ShadowedByMacos)
            );
            if by_macos && ctx.cfg.is_default_prefix() {
                let hint = keg_only_path_message(ctx, &keg);
                output::opoo(&format!(
                    "Refusing to link macOS provided/shadowed software: {short}{}",
                    if hint.is_empty() {
                        String::new()
                    } else {
                        format!("\n{}", hint.trim())
                    }
                ));
                continue;
            }
            if !args.force && !is_versioned_keg_only(formula.as_ref()) {
                output::opoo(&format!(
                    "{short} is keg-only and must be linked with `--force`."
                ));
                print!("{}", keg_only_path_message(ctx, &keg));
                continue;
            }
        }

        print!("Linking {}... ", keg.path.display());
        if ctx.verbose {
            println!();
        }
        output::flush();
        let globs = formula
            .as_ref()
            .map(|f| f.link_overwrite_paths.clone())
            .unwrap_or_default();
        match crate::keg::link::link(&ctx.cfg, &keg, &globs, opts) {
            Ok(n) => println!("{n} symlinks created."),
            Err(e) => {
                println!();
                return Err(e);
            }
        }
        if formula.as_ref().is_some_and(|f| f.is_keg_only())
            && !is_versioned_keg_only(formula.as_ref())
        {
            print!("{}", keg_only_path_message(ctx, &keg));
        }
    }
    Ok(())
}

fn is_versioned_keg_only(formula: Option<&crate::model::FormulaEntry>) -> bool {
    formula
        .and_then(|f| f.keg_only())
        .map(|(reason, _)| reason == crate::model::KegOnly::VersionedFormula)
        .unwrap_or(false)
}

/// `Link#puts_keg_only_path_message`.
fn keg_only_path_message(ctx: &Ctx, keg: &crate::keg::Keg) -> String {
    let bin = keg.path.join("bin").is_dir();
    let sbin = keg.path.join("sbin").is_dir();
    if !bin && !sbin {
        return String::new();
    }
    let opt = ctx.cfg.opt_record(&keg.name);
    let mut out =
        "\nIf you need to have this software first in your PATH instead consider running:\n"
            .to_string();
    if bin {
        out.push_str(&format!(
            "  {}\n",
            prepend_path_in_profile(&opt.join("bin").to_string_lossy())
        ));
    }
    if sbin {
        out.push_str(&format!(
            "  {}\n",
            prepend_path_in_profile(&opt.join("sbin").to_string_lossy())
        ));
    }
    out
}

/// `Utils::Shell.prepend_path_in_profile` for the user's `$SHELL`.
fn prepend_path_in_profile(path: &str) -> String {
    let shell = std::env::var("SHELL")
        .ok()
        .and_then(|s| s.rsplit('/').next().map(str::to_string))
        .unwrap_or_else(|| "sh".to_string());
    let profile = match shell.as_str() {
        "zsh" => "~/.zshrc",
        "csh" => "~/.cshrc",
        "tcsh" => "~/.tcshrc",
        "fish" => "~/.config/fish/config.fish",
        "ksh" | "mksh" => "~/.kshrc",
        _ => "~/.profile",
    };
    match shell.as_str() {
        "fish" => format!("fish_add_path {path}"),
        "csh" | "tcsh" => format!("echo 'setenv PATH {path}:$PATH' >> {profile}"),
        _ => format!("echo 'export PATH=\"{path}:$PATH\"' >> {profile}"),
    }
}

#[derive(Args, Debug)]
pub struct PinArgs {
    #[arg(value_name = "formula|cask", required = true)]
    pub names: Vec<String>,
    // Homebrew: `conflicts "--formula", "--cask"`.
    #[arg(long, visible_alias = "formulae", conflicts_with = "cask")]
    pub formula: bool,
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
}

pub fn pin(ctx: &Ctx, args: &PinArgs, pin_it: bool) -> Result<()> {
    let index = ctx.index()?;
    for name in &args.names {
        // `NamedArgs#to_resolved_formulae_to_casks`: a bare name may be
        // either, and `--cask` makes every name a cask.
        if args.cask || (!args.formula && is_cask_name(ctx, name)) {
            pin_cask(ctx, name, pin_it)?;
            continue;
        }
        // Pinning acts on an installed keg, so the rack's receipt picks the tap.
        let formula = resolve::resolve_installed(&ctx.cfg, index, name)?;
        let action = if pin_it { "pin" } else { "unpin" };
        resolve::check_installed_tap(&ctx.cfg, name, &formula, action)?;
        let full = formula.full_name();
        let pinned = crate::keg::is_pinned(&ctx.cfg, &formula.name);
        // `Formula#pinnable?`: there has to be a keg to pin.
        let pinnable = !crate::keg::installed_kegs(&ctx.cfg, &formula.name).is_empty();
        if pin_it {
            if pinned {
                output::opoo(&format!("{full} already pinned"));
            } else if !pinnable {
                output::ofail(&format!("{full} not installed"));
            } else {
                crate::ops::pin::pin(&ctx.cfg, &formula.name)?;
            }
        } else if pinned {
            crate::ops::pin::unpin(&ctx.cfg, &formula.name)?;
        } else if !pinnable {
            // `cmd/unpin.rb` uses `onoe`, which does not set the exit status.
            output::onoe(&format!("{full} not installed"));
        } else {
            output::opoo(&format!("{full} not pinned"));
        }
    }
    Ok(())
}

/// Whether a bare name means a cask here: only an installed one, so a name a
/// formula also carries keeps meaning the formula (`to_resolved_formulae_to_casks`
/// resolves formulae first).
fn is_cask_name(ctx: &Ctx, name: &str) -> bool {
    crate::keg::installed_kegs(&ctx.cfg, name.rsplit('/').next().unwrap_or(name)).is_empty()
        && crate::cask::installed_cask(&ctx.cfg, name).is_some()
}

/// `cmd/pin.rb` and `cmd/unpin.rb` for a cask.
fn pin_cask(ctx: &Ctx, name: &str, pin_it: bool) -> Result<()> {
    let index = ctx.index()?;
    let cask = resolve::resolve_cask(&ctx.cfg, index, name)?;
    let full = cask.full_token();
    let installed = crate::cask::installed_cask(&ctx.cfg, &cask.token);
    let pinned = crate::cask::is_pinned(&ctx.cfg, &cask.token);
    if pin_it {
        if pinned {
            output::opoo(&format!("{full} already pinned"));
        } else if let Some(installed) = &installed {
            crate::cask::pin(&ctx.cfg, installed)?;
            if cask.auto_updates {
                output::opoo(&format!(
                    "{full} has `auto_updates true` and may update itself outside Homebrew despite being pinned."
                ));
            }
        } else {
            output::ofail(&format!("{full} not installed"));
        }
        return Ok(());
    }
    // `cmd/unpin.rb` drops a dangling record too.
    if pinned || crate::cask::pin_path(&ctx.cfg, &cask.token).is_symlink() {
        crate::cask::unpin(&ctx.cfg, &cask.token)?;
    } else if installed.is_none() {
        output::onoe(&format!("{full} not installed"));
    } else {
        output::opoo(&format!("{full} not pinned"));
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct PostinstallArgs {
    #[arg(value_name = "formula", required = true)]
    pub names: Vec<String>,
}

pub fn postinstall(ctx: &Ctx, args: &PostinstallArgs) -> Result<()> {
    let index = ctx.index()?;
    for name in &args.names {
        let formula = resolve::resolve_installed(&ctx.cfg, index, name)?;
        resolve::check_installed_tap(&ctx.cfg, name, &formula, "postinstall")?;
        let keg = latest_keg(ctx, &formula.name)?;
        // A Ruby `post_install` is the Ruby `brew`'s job; `postinstall` is a
        // whole command, so it hands the invocation over rather than
        // shelling out mid-run.
        if crate::ops::postinstall::needs_ruby_post_install(&formula) {
            return ctx.delegate(&format!(
                "`{}`'s post_install needs the Ruby formula DSL",
                formula.full_name()
            ));
        }
        crate::ops::postinstall::run_post_install(&ctx.cfg, &formula, &keg)?;
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct FetchArgs {
    #[arg(value_name = "formula|cask", required = true)]
    pub names: Vec<String>,
    // Homebrew: `conflicts "--formula", "--cask"`.
    #[arg(long, visible_alias = "formulae", conflicts_with = "cask")]
    pub formula: bool,
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
    #[arg(short = 'f', long)]
    pub force: bool,
    /// Also download the dependencies.
    #[arg(long)]
    pub deps: bool,
    /// Do not quarantine a cask's download.
    #[arg(long)]
    pub no_quarantine: bool,
    /// Quarantine a cask's download (the default).
    #[arg(long, conflicts_with = "no_quarantine")]
    pub quarantine: bool,
}

pub fn fetch(ctx: &Ctx, args: &FetchArgs) -> Result<()> {
    let index = ctx.index()?;
    let (formulae, casks) = partition(ctx, &args.names, kind_of(args.formula, args.cask), None)?;
    if !formulae.is_empty() {
        crate::ops::install::fetch_formulae(&ctx.cfg, index, &formulae, args.deps, args.force)?;
    }
    if !casks.is_empty() {
        let no_quarantine = if args.quarantine {
            false
        } else {
            args.no_quarantine
                || crate::cask::config::bool_flag(&ctx.cfg.cask_opts, "quarantine") == Some(false)
        };
        crate::cask::install::fetch_casks(&ctx.cfg, &casks, args.force, no_quarantine)?;
    }
    Ok(())
}
