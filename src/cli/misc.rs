//! `config`, `commands`, `shellenv`, `help`, `update` and the `--path`
//! pseudo-commands (`--prefix`, `--cellar`, `--cache`, `--repository`,
//! `--caskroom`, `--version`, `--env`, `--taps`).

use crate::error::{Error, Result};
use crate::output;
use crate::resolve;

use super::Ctx;
use super::commands::{CommandsArgs, HelpArgs, ShellenvArgs, UpdateArgs};

/// Every command fastbrew knows, for `commands` and `help`.
pub const BUILTIN_COMMANDS: &[&str] = &[
    "--cache",
    "--caskroom",
    "--cellar",
    "--env",
    "--prefix",
    "--repository",
    "--taps",
    "--version",
    "autoremove",
    "cleanup",
    "commands",
    "config",
    "deps",
    "desc",
    "fetch",
    "help",
    "home",
    "info",
    "install",
    "leaves",
    "link",
    "list",
    "missing",
    "options",
    "outdated",
    "pin",
    "postinstall",
    "reinstall",
    "search",
    "services",
    "shellenv",
    "tap",
    "uninstall",
    "unlink",
    "unpin",
    "untap",
    "update",
    "upgrade",
    "uses",
    "which-formula",
];

pub fn is_path_command(name: &str) -> bool {
    matches!(
        name,
        "--prefix"
            | "--cellar"
            | "--cache"
            | "--repository"
            | "--caskroom"
            | "--version"
            | "--env"
            | "--taps"
    )
}

pub fn run_path_command(ctx: &Ctx, name: &str, args: &[String]) -> Result<()> {
    let cfg = &ctx.cfg;
    let named: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    match name {
        "--version" => {
            println!("Homebrew {}", crate::HOMEBREW_COMPAT_VERSION);
            println!("fastbrew {}", crate::FASTBREW_VERSION);
        }
        "--prefix" => {
            if named.is_empty() {
                println!("{}", cfg.prefix.display());
            } else {
                let index = ctx.index()?;
                let installed_only = args.iter().any(|a| a == "--installed");
                let mut missing: Vec<String> = Vec::new();
                for n in named {
                    let f = resolve::resolve_formula(cfg, index, n)?;
                    let opt = cfg.opt_record(&f.name);
                    if installed_only && !opt.exists() {
                        missing.push(f.name.clone());
                        continue;
                    }
                    println!("{}", opt.display());
                }
                if !missing.is_empty() {
                    return Err(Error::user(format!(
                        "The following formulae are not installed:\n{}",
                        missing.join(" ")
                    )));
                }
            }
        }
        "--cellar" => {
            if named.is_empty() {
                println!("{}", cfg.cellar.display());
            } else {
                let index = ctx.index()?;
                for n in named {
                    let f = resolve::resolve_formula(cfg, index, n)?;
                    println!("{}", cfg.rack(&f.name).display());
                }
            }
        }
        "--cache" => {
            if named.is_empty() {
                println!("{}", cfg.cache.display());
            } else {
                let index = ctx.index()?;
                for n in named {
                    let f = resolve::resolve_formula(cfg, index, n)?;
                    println!(
                        "{}",
                        cfg.cache_downloads()
                            .join(format!("{}--{}", f.name, f.pkg_version()))
                            .display()
                    );
                }
            }
        }
        "--repository" => {
            if named.is_empty() {
                println!("{}", cfg.repository.display());
            } else {
                for n in named {
                    let Some((user, repo)) = n.split_once('/') else {
                        return Err(Error::user(format!("Invalid tap name: {n}")));
                    };
                    let repo = repo.trim_start_matches("homebrew-");
                    println!(
                        "{}",
                        cfg.taps_dir()
                            .join(user.to_lowercase())
                            .join(format!("homebrew-{}", repo.to_lowercase()))
                            .display()
                    );
                }
            }
        }
        "--caskroom" => {
            if named.is_empty() {
                println!("{}", cfg.caskroom().display());
            } else {
                for n in named {
                    println!("{}/{n}", cfg.caskroom().display());
                }
            }
        }
        "--taps" => println!("{}", cfg.taps_dir().display()),
        "--env" => {
            println!("export HOMEBREW_PREFIX=\"{}\"", cfg.prefix.display());
            println!("export HOMEBREW_CELLAR=\"{}\"", cfg.cellar.display());
            println!(
                "export HOMEBREW_REPOSITORY=\"{}\"",
                cfg.repository.display()
            );
        }
        other => return Err(Error::user(format!("Unknown command: {other}"))),
    }
    Ok(())
}

pub fn config(ctx: &Ctx) -> Result<()> {
    let cfg = &ctx.cfg;
    let host = crate::platform::Host::detect();
    println!("HOMEBREW_VERSION: {}", crate::HOMEBREW_COMPAT_VERSION);
    println!("FASTBREW_VERSION: {}", crate::FASTBREW_VERSION);
    println!("ORIGIN: {}", crate::config::DEFAULT_BREW_GIT_REMOTE);
    println!("HEAD: (none)");
    println!("Last commit: never");
    println!("Branch: (none)");
    match ctx.index() {
        Ok(index) => {
            let meta = index.metadata();
            println!("Core tap JSON: {}", api_date(meta.generated_at));
            println!("Core cask tap JSON: {}", api_date(meta.generated_at));
        }
        Err(_) => {
            println!("Core tap: N/A");
            println!("Core cask tap: N/A");
        }
    }
    println!("HOMEBREW_PREFIX: {}", cfg.prefix.display());
    println!("HOMEBREW_REPOSITORY: {}", cfg.repository.display());
    println!("HOMEBREW_CELLAR: {}", cfg.cellar.display());
    println!("HOMEBREW_CACHE: {}", cfg.cache.display());
    println!("HOMEBREW_LOGS: {}", cfg.logs.display());
    println!("HOMEBREW_TEMP: {}", cfg.temp.display());
    if cfg.no_auto_update {
        println!("HOMEBREW_NO_AUTO_UPDATE: set");
    }
    println!("CPU: {}", host.arch.as_str());
    match host.macos {
        Some(v) => println!("macOS: {v}-{}", host.arch.as_str()),
        None => println!("Linux: {}", host.arch.as_str()),
    }
    println!("Bottle tag: {}", ctx.tag);
    Ok(())
}

fn api_date(generated_at: u64) -> String {
    use chrono::{TimeZone, Utc};
    match Utc.timestamp_opt(generated_at as i64, 0).single() {
        Some(t) => t.format("%d %b %H:%M UTC").to_string(),
        None => "N/A".to_string(),
    }
}

pub fn commands(ctx: &Ctx, args: &CommandsArgs) -> Result<()> {
    let mut names: Vec<String> = BUILTIN_COMMANDS.iter().map(|s| s.to_string()).collect();
    if args.include_aliases {
        names.extend(super::COMMAND_ALIASES.iter().map(|(a, _)| a.to_string()));
        names.sort();
        names.dedup();
    }
    if args.quiet_list || ctx.quiet {
        output::print_columns(&names);
        return Ok(());
    }
    output::ohai("Built-in commands");
    output::print_columns(&names);
    Ok(())
}

pub fn shellenv(ctx: &Ctx, args: &ShellenvArgs) -> Result<()> {
    let cfg = &ctx.cfg;
    let prefix = cfg.prefix.display().to_string();
    let cellar = cfg.cellar.display().to_string();
    let repository = cfg.repository.display().to_string();
    let shell = args
        .shell
        .clone()
        .or_else(|| {
            std::env::var("SHELL")
                .ok()
                .and_then(|s| s.rsplit('/').next().map(str::to_string))
        })
        .unwrap_or_else(|| "sh".to_string());
    let shell = shell.trim_start_matches('-');

    match shell {
        "fish" => {
            println!("set --global --export HOMEBREW_PREFIX \"{prefix}\";");
            println!("set --global --export HOMEBREW_CELLAR \"{cellar}\";");
            println!("set --global --export HOMEBREW_REPOSITORY \"{repository}\";");
            println!("fish_add_path --global --move --path \"{prefix}/bin\" \"{prefix}/sbin\";");
            println!(
                "if test -n \"$MANPATH\"; set --global --export MANPATH (string replace --regex '^:*(.*?):*$' ':$1' -- \"$MANPATH\"); end;"
            );
            println!(
                "if not set --query INFOPATH; set INFOPATH ''; end; set --global --export INFOPATH \"{prefix}/share/info\" $INFOPATH;"
            );
        }
        "csh" | "tcsh" => {
            println!("setenv HOMEBREW_PREFIX {prefix};");
            println!("setenv HOMEBREW_CELLAR {cellar};");
            println!("setenv HOMEBREW_REPOSITORY {repository};");
            println!("setenv PATH \"{prefix}/bin:{prefix}/sbin:$PATH\";");
            println!(
                "test ${{?MANPATH}} -eq 1 && test -n \"${{MANPATH}}\" && setenv MANPATH :`printf '%s' \"${{MANPATH}}\" | /usr/bin/sed -e 's/^:*//' -e 's/:*$//'`;"
            );
            println!("test ${{?INFOPATH}} -eq 1 || setenv INFOPATH '';");
            println!("setenv INFOPATH \"{prefix}/share/info:${{INFOPATH}}\";");
        }
        _ => {
            println!("export HOMEBREW_PREFIX=\"{prefix}\";");
            println!("export HOMEBREW_CELLAR=\"{cellar}\";");
            println!("export HOMEBREW_REPOSITORY=\"{repository}\";");
            if shell == "zsh" {
                println!("fpath[1,0]=\"{prefix}/share/zsh/site-functions\";");
                println!("export FPATH;");
            }
            println!("export PATH=\"{prefix}/bin:{prefix}/sbin${{PATH+:$PATH}}\";");
            println!(
                "[ -z \"${{MANPATH-}}\" ] || {{ export MANPATH=\"${{MANPATH%\"${{MANPATH##*[!:]}}\"}}\"; export MANPATH=\":${{MANPATH#\"${{MANPATH%%[!:]*}}\"}}\"; }};"
            );
            println!("export INFOPATH=\"{prefix}/share/info:${{INFOPATH:-}}\";");
        }
    }
    Ok(())
}

pub fn help(ctx: &Ctx, args: &HelpArgs) -> Result<()> {
    if let Some(command) = &args.command {
        let normalized = super::resolve_alias(command);
        if !BUILTIN_COMMANDS.contains(&normalized) {
            return ctx.delegate(&format!("`help {command}` is not implemented by fastbrew"));
        }
    }
    println!(
        "Example usage:
  fastbrew search [TEXT|/REGEX/]
  fastbrew info [FORMULA|CASK...]
  fastbrew install FORMULA|CASK...
  fastbrew update
  fastbrew upgrade [FORMULA|CASK...]
  fastbrew uninstall FORMULA|CASK...
  fastbrew list [FORMULA|CASK...]

Troubleshooting:
  fastbrew config
  fastbrew doctor
  fastbrew install --verbose --debug FORMULA|CASK

Further help:
  fastbrew commands
  fastbrew help [COMMAND]"
    );
    Ok(())
}

pub fn update(ctx: &Ctx, args: &UpdateArgs) -> Result<()> {
    let report = crate::update::update(&ctx.cfg, args.force, ctx.quiet, args.auto_update)?;
    crate::update::print_report(&ctx.cfg, &report, ctx.quiet);
    Ok(())
}

/// `Utils::Browser.open`: `$HOMEBREW_BROWSER`, `$BROWSER`, then `open`.
pub fn open_url(url: &str) -> Result<()> {
    let browser = std::env::var("HOMEBREW_BROWSER")
        .or_else(|_| std::env::var("BROWSER"))
        .unwrap_or_else(|_| {
            if cfg!(target_os = "linux") {
                "xdg-open".to_string()
            } else {
                "open".to_string()
            }
        });
    let status = std::process::Command::new(&browser).arg(url).status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => Err(Error::user(format!("Failed to open {url} with {browser}"))),
    }
}
