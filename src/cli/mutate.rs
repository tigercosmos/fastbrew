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
fn partition(ctx: &Ctx, names: &[String], kind: Kind) -> Result<(Vec<String>, Vec<String>)> {
    let index = ctx.index()?;
    let mut formulae = Vec::new();
    let mut casks = Vec::new();
    for name in names {
        match resolve::resolve(&ctx.cfg, index, name, kind)? {
            resolve::Resolved::Formula(f) => formulae.push(f.name),
            resolve::Resolved::Cask(c) => casks.push(c.token),
        }
    }
    Ok((formulae, casks))
}

#[derive(Args, Debug)]
pub struct InstallArgs {
    #[arg(value_name = "formula|cask", required = true)]
    pub names: Vec<String>,
    #[arg(long, visible_alias = "formulae")]
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
    crate::update::auto_update_if_needed(&ctx.cfg, "install");
    let (formulae, casks) = partition(ctx, &args.names, kind_of(args.formula, args.cask))?;
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
        crate::ops::install::install_formulae(&ctx.cfg, index, &formulae, &opts)?;
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
    CaskInstallOptions {
        force: args.force,
        adopt: args.adopt,
        skip_cask_deps: args.skip_cask_deps,
        dry_run: args.dry_run,
        quiet: ctx.quiet,
        verbose: ctx.verbose,
        reinstall,
        explicit_dir_flags,
    }
}

#[derive(Args, Debug)]
pub struct UpgradeArgs {
    #[arg(value_name = "formula|cask")]
    pub names: Vec<String>,
    #[arg(long, visible_alias = "formulae")]
    pub formula: bool,
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    #[arg(short = 'f', long)]
    pub force: bool,
    #[arg(short = 'g', long)]
    pub greedy: bool,
}

pub fn upgrade(ctx: &Ctx, args: &UpgradeArgs) -> Result<()> {
    crate::update::auto_update_if_needed(&ctx.cfg, "upgrade");
    let index = ctx.index()?;
    let (formulae, casks) = partition(ctx, &args.names, kind_of(args.formula, args.cask))?;
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
        let opts = CaskInstallOptions {
            force: args.force,
            dry_run: args.dry_run,
            quiet: ctx.quiet,
            verbose: ctx.verbose,
            ..Default::default()
        };
        crate::cask::install::upgrade_casks(&ctx.cfg, index, &casks, args.greedy, &opts)?;
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct UninstallArgs {
    #[arg(value_name = "formula|cask", required = true)]
    pub names: Vec<String>,
    #[arg(long, visible_alias = "formulae")]
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
    let (formulae, casks) = partition(ctx, &args.names, kind_of(args.formula, args.cask))?;
    if !formulae.is_empty() {
        let opts = UninstallOptions {
            force: args.force,
            ignore_dependencies: args.ignore_dependencies,
            dry_run: args.dry_run,
        };
        crate::ops::uninstall::uninstall_formulae(&ctx.cfg, index, &formulae, &opts)?;
    }
    if !casks.is_empty() {
        crate::cask::uninstall::uninstall_casks(&ctx.cfg, index, &casks, args.zap, args.force)?;
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
    #[arg(long)]
    pub overwrite: bool,
    #[arg(short = 'f', long)]
    pub force: bool,
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

pub fn link(ctx: &Ctx, args: &LinkArgs, link_it: bool) -> Result<()> {
    let index = ctx.index()?;
    let opts = crate::keg::link::LinkOptions {
        overwrite: args.overwrite,
        dry_run: args.dry_run,
        verbose: ctx.verbose,
    };
    for name in &args.names {
        let formula = resolve::resolve_formula(&ctx.cfg, index, name)?;
        let Some(keg) = crate::keg::latest_keg(&ctx.cfg, &formula.name) else {
            return Err(Error::user(format!(
                "No such keg: {}",
                ctx.cfg.rack(&formula.name).display()
            )));
        };
        if link_it {
            crate::keg::link::link(&ctx.cfg, &keg, &formula.link_overwrite_paths, opts)?;
        } else {
            crate::keg::link::unlink(&ctx.cfg, &keg, opts)?;
        }
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct PinArgs {
    #[arg(value_name = "formula", required = true)]
    pub names: Vec<String>,
}

pub fn pin(ctx: &Ctx, args: &PinArgs, pin_it: bool) -> Result<()> {
    let index = ctx.index()?;
    for name in &args.names {
        let formula = resolve::resolve_formula(&ctx.cfg, index, name)?;
        if pin_it {
            crate::ops::pin::pin(&ctx.cfg, &formula.name)?;
        } else {
            crate::ops::pin::unpin(&ctx.cfg, &formula.name)?;
        }
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
        let formula = resolve::resolve_formula(&ctx.cfg, index, name)?;
        let Some(keg) = crate::keg::latest_keg(&ctx.cfg, &formula.name) else {
            return Err(Error::user(format!(
                "No such keg: {}",
                ctx.cfg.rack(&formula.name).display()
            )));
        };
        crate::ops::postinstall::run_post_install(&ctx.cfg, &formula, &keg)?;
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct FetchArgs {
    #[arg(value_name = "formula|cask", required = true)]
    pub names: Vec<String>,
    #[arg(long, visible_alias = "formulae")]
    pub formula: bool,
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
    #[arg(short = 'f', long)]
    pub force: bool,
    /// Also download the dependencies.
    #[arg(long)]
    pub deps: bool,
}

pub fn fetch(ctx: &Ctx, args: &FetchArgs) -> Result<()> {
    let index = ctx.index()?;
    let (formulae, casks) = partition(ctx, &args.names, kind_of(args.formula, args.cask))?;
    if !formulae.is_empty() {
        crate::ops::install::fetch_formulae(&ctx.cfg, index, &formulae, args.deps, args.force)?;
    }
    if !casks.is_empty() {
        crate::cask::install::fetch_casks(&ctx.cfg, index, &casks, args.force)?;
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct TapArgs {
    #[arg(value_name = "user/repo")]
    pub name: Option<String>,
    #[arg(value_name = "URL")]
    pub url: Option<String>,
    #[arg(long)]
    pub force: bool,
}

pub fn tap(ctx: &Ctx, args: &TapArgs) -> Result<()> {
    let Some(name) = &args.name else {
        for t in crate::tap::installed_taps(&ctx.cfg) {
            println!("{}", t.name());
        }
        return Ok(());
    };
    crate::update::auto_update_if_needed(&ctx.cfg, "tap");
    crate::tap::tap(&ctx.cfg, name, args.url.as_deref(), args.force, ctx.quiet)
}

#[derive(Args, Debug)]
pub struct UntapArgs {
    #[arg(value_name = "user/repo", required = true)]
    pub names: Vec<String>,
    #[arg(long)]
    pub force: bool,
}

pub fn untap(ctx: &Ctx, args: &UntapArgs) -> Result<()> {
    for name in &args.names {
        crate::tap::untap(&ctx.cfg, name, args.force)?;
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct ServicesArgs {
    #[arg(value_name = "subcommand")]
    pub subcommand: Option<String>,
    #[arg(value_name = "formula")]
    pub names: Vec<String>,
    #[arg(long)]
    pub json: bool,
    /// Run the service as root (system domain).
    #[arg(long)]
    pub sudo_service_user: bool,
}

pub fn services(ctx: &Ctx, args: &ServicesArgs) -> Result<()> {
    let sudo = args.sudo_service_user;
    let sub = args.subcommand.as_deref().unwrap_or("list");
    match sub {
        "list" | "ls" => {
            let list = crate::services::list(&ctx.cfg)?;
            if args.json {
                let doc: Vec<serde_json::Value> = list
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "status": format!("{:?}", s.status).to_lowercase(),
                            "user": s.user,
                            "file": s.file.as_ref().map(|p| p.display().to_string()),
                            "exit_code": s.exit_code,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
            } else {
                println!("Name Status User File");
                for s in list {
                    println!(
                        "{} {:?} {} {}",
                        s.name,
                        s.status,
                        s.user.unwrap_or_default(),
                        s.file.map(|p| p.display().to_string()).unwrap_or_default()
                    );
                }
            }
            Ok(())
        }
        "info" => {
            for name in &args.names {
                let info = crate::services::info(&ctx.cfg, name)?;
                println!("{} ({}): {:?}", info.name, info.label, info.status);
            }
            Ok(())
        }
        "start" => run_each(ctx, &args.names, sudo, crate::services::start),
        "stop" => run_each(ctx, &args.names, sudo, crate::services::stop),
        "restart" => run_each(ctx, &args.names, sudo, crate::services::restart),
        "run" => run_each(ctx, &args.names, sudo, crate::services::run),
        "kill" => run_each(ctx, &args.names, sudo, crate::services::kill),
        "cleanup" => crate::services::cleanup(&ctx.cfg),
        other => Err(Error::user(format!("Unknown subcommand: {other}"))),
    }
}

fn run_each(
    ctx: &Ctx,
    names: &[String],
    sudo: bool,
    f: fn(&crate::config::Config, &str, bool) -> Result<()>,
) -> Result<()> {
    if names.is_empty() {
        return Err(Error::user("This command requires a formula argument"));
    }
    for name in names {
        f(&ctx.cfg, name, sudo)?;
    }
    Ok(())
}
