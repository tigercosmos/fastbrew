//! `services` and its subcommands.
//!
//! Port of `cmd/services.rb` and `services/subcommand/*.rb`: the subcommand
//! names and aliases, `--all`, `--json`, `--sudo-service-user` and the error
//! wording of `Services::Cli`. The work itself is in `crate::services`.

use clap::Args;

use crate::error::{Error, Result};
use crate::output;
use crate::resolve;
use crate::services::{self, ServiceInfo};

use super::Ctx;

#[derive(Args, Debug)]
pub struct ServicesArgs {
    #[arg(value_name = "subcommand")]
    pub subcommand: Option<String>,
    #[arg(value_name = "formula")]
    pub names: Vec<String>,
    /// Operate on every managed service.
    #[arg(long)]
    pub all: bool,
    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
    /// When run as root on macOS, run the service(s) as this user.
    // `cmd/services.rb` declares `flag "--sudo-service-user="`, so the value
    // is required: a bare `--sudo-service-user` is a usage error.
    #[arg(long, value_name = "user", require_equals = true)]
    pub sudo_service_user: Option<String>,
    /// Use the service file at this location (`start`, `run`, `restart`).
    #[arg(long, value_name = "path")]
    pub file: Option<String>,
    /// `stop`: leave the service registered to launch at login.
    #[arg(long)]
    pub keep: bool,
    /// `stop`: do not wait for the service to finish stopping.
    #[arg(long)]
    pub no_wait: bool,
    /// `stop`: wait at most this many seconds.
    #[arg(long, value_name = "seconds")]
    pub max_wait: Option<f64>,
}

/// `AbstractSubcommand` names with their aliases.
fn canonical(sub: &str) -> Option<&'static str> {
    const SUBCOMMANDS: &[(&str, &[&str])] = &[
        ("list", &["ls"]),
        ("info", &["i"]),
        ("start", &["launch", "load", "s", "l"]),
        ("stop", &["unload", "terminate", "term", "t", "u"]),
        ("restart", &["relaunch", "reload", "r"]),
        ("run", &[]),
        ("kill", &["k"]),
        ("cleanup", &["clean", "cl", "rm"]),
    ];
    SUBCOMMANDS
        .iter()
        .find(|(name, aliases)| *name == sub || aliases.contains(&sub))
        .map(|(name, _)| *name)
}

pub fn services(ctx: &Ctx, args: &ServicesArgs) -> Result<()> {
    // The default subcommand is `list`; a bare formula name is not one.
    let (sub, names): (&str, Vec<String>) = match args.subcommand.as_deref() {
        None => ("list", args.names.clone()),
        Some(raw) => match canonical(raw) {
            Some(name) => (name, args.names.clone()),
            None => {
                return Err(Error::user(format!(
                    "Unknown subcommand: {raw}\nUsage: fastbrew services [list|info|start|stop|restart|run|kill|cleanup]"
                )));
            }
        },
    };
    // `Subcommand.dispatch`: the flag is only usable as root, and only on
    // macOS, where launchd has a system domain to put the service in.
    let sudo_user = match args.sudo_service_user.as_deref() {
        None => None,
        Some(user) if user.trim().is_empty() => {
            return Err(Error::user(
                "Invalid usage: `fastbrew services --sudo-service-user` requires a username.",
            ));
        }
        Some(user) => {
            if !services::launchd::is_root() {
                return Err(Error::user(
                    "`fastbrew services --sudo-service-user` is supported only when running as root!",
                ));
            }
            Some(user)
        }
    };

    // Flags whose semantics fastbrew does not implement would change what the
    // command does, so they go to the Ruby `brew` rather than being ignored.
    for (set, flag) in [
        (args.file.is_some(), "--file"),
        (args.keep, "--keep"),
        (args.no_wait, "--no-wait"),
        (args.max_wait.is_some(), "--max-wait"),
    ] {
        if set {
            return ctx.delegate(&format!(
                "`services {sub} {flag}` is not implemented by fastbrew"
            ));
        }
    }

    match sub {
        "list" => list(ctx, args),
        "info" => info(ctx, args, &names),
        "cleanup" => services::cleanup(&ctx.cfg),
        _ => act(ctx, args, sub, &names, sudo_user),
    }
}

fn list(ctx: &Ctx, args: &ServicesArgs) -> Result<()> {
    let list = services::list(&ctx.cfg)?;
    if list.is_empty() {
        // `ListSubcommand#run`: the hint only goes to a terminal.
        if output::stderr_is_tty() {
            output::opoo(&format!(
                "No services available to control with `{}`",
                services::BIN
            ));
        }
        if args.json {
            println!("[]");
        }
        return Ok(());
    }
    if args.json {
        println!("{}", services::list_json(&list));
    } else {
        print!("{}", services::format_list_table(&list));
    }
    Ok(())
}

fn info(ctx: &Ctx, args: &ServicesArgs, names: &[String]) -> Result<()> {
    let infos: Vec<ServiceInfo> = if args.all {
        services::available_services(&ctx.cfg, None, false)
    } else {
        check(names)?;
        let index = ctx.index()?;
        let mut out = Vec::new();
        for name in names {
            // `Formulary.factory` first: an unknown name is the error shown.
            let formula = resolve::resolve_formula(&ctx.cfg, index, name)?;
            out.push(services::info_or_default(&ctx.cfg, &formula.name));
        }
        out
    };
    if args.json {
        println!("{}", services::info_json(&infos));
        return Ok(());
    }
    for i in &infos {
        print!("{}", services::format_info(i, ctx.verbose));
    }
    Ok(())
}

/// `Cli.check!`: `--all` or at least one formula.
fn check(names: &[String]) -> Result<()> {
    if names.is_empty() {
        return Err(Error::user(
            "Formula(e) missing, please provide a formula name or use `--all`.",
        ));
    }
    Ok(())
}

fn act(
    ctx: &Ctx,
    args: &ServicesArgs,
    sub: &str,
    names: &[String],
    sudo_user: services::SudoUser<'_>,
) -> Result<()> {
    let targets: Vec<String> = if args.all {
        // `targets`: `start` only touches what is not loaded, `stop` only
        // what is.
        let loaded = match sub {
            "start" => Some(false),
            "stop" => Some(true),
            _ => None,
        };
        services::available_services(&ctx.cfg, loaded, sudo_user.is_none())
            .into_iter()
            .map(|s| s.name)
            .collect()
    } else {
        check(names)?;
        // `Cli.targets` builds a `FormulaWrapper` around `Formulary.factory`,
        // so an unknown name fails in resolution, not with "not installed".
        let index = ctx.index()?;
        let mut resolved = Vec::with_capacity(names.len());
        for name in names {
            resolved.push(resolve::resolve_formula(&ctx.cfg, index, name)?.name);
        }
        resolved
    };

    let action = match sub {
        "start" => services::start,
        "stop" => services::stop,
        "restart" => services::restart,
        "run" => services::run,
        "kill" => services::kill,
        other => {
            return Err(Error::user(format!("Unknown subcommand: {other}")));
        }
    };
    for name in &targets {
        action(&ctx.cfg, name, sudo_user)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subcommand_aliases_match_homebrew() {
        assert_eq!(canonical("ls"), Some("list"));
        assert_eq!(canonical("list"), Some("list"));
        assert_eq!(canonical("i"), Some("info"));
        assert_eq!(canonical("launch"), Some("start"));
        assert_eq!(canonical("term"), Some("stop"));
        assert_eq!(canonical("reload"), Some("restart"));
        assert_eq!(canonical("k"), Some("kill"));
        assert_eq!(canonical("rm"), Some("cleanup"));
        assert_eq!(canonical("run"), Some("run"));
        assert_eq!(canonical("nope"), None);
    }
}
