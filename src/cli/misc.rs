//! `config`, `commands`, `completions`, `shellenv`, `help`, `update` and the
//! `--path` pseudo-commands (`--prefix`, `--cellar`, `--cache`,
//! `--repository`, `--caskroom`, `--version`, `--env`, `--taps`).

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::output;
use crate::resolve;
use crate::tap::Tap;

use super::Ctx;
use super::commands::{CommandsArgs, CompletionsArgs, HelpArgs, ShellenvArgs, UpdateArgs};

/// Every command fastbrew runs natively, for `commands` and `help`.
///
/// `Commands.internal_commands` is the list of files under `cmd/`; this is
/// the same list restricted to what fastbrew implements. Everything else is
/// delegated and therefore an *external* command from fastbrew's point of
/// view.
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
    "completions",
    "config",
    "deps",
    "desc",
    "doctor",
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
    "tap-info",
    "uninstall",
    "unlink",
    "unpin",
    "untap",
    "update",
    "upgrade",
    "uses",
    "which-formula",
];

/// One line of usage per built-in command, for `help <command>`.
///
/// Homebrew prints the command's `usage_banner` plus its description; these
/// are the same banners with `brew` replaced by `fastbrew`.
pub const COMMAND_USAGE: &[(&str, &str)] = &[
    ("--cache", "fastbrew --cache [FORMULA|CASK...]"),
    ("--caskroom", "fastbrew --caskroom [CASK...]"),
    ("--cellar", "fastbrew --cellar [FORMULA...]"),
    ("--env", "fastbrew --env"),
    ("--prefix", "fastbrew --prefix [--installed] [FORMULA...]"),
    ("--repository", "fastbrew --repository [TAP...]"),
    ("--taps", "fastbrew --taps"),
    ("--version", "fastbrew --version"),
    ("autoremove", "fastbrew autoremove [--dry-run]"),
    (
        "cleanup",
        "fastbrew cleanup [--prune=DAYS] [--dry-run] [-s] [--prune-prefix] [FORMULA|CASK...]",
    ),
    (
        "commands",
        "fastbrew commands [--quiet] [--include-aliases]",
    ),
    ("completions", "fastbrew completions [link|unlink|state]"),
    ("config", "fastbrew config"),
    (
        "deps",
        "fastbrew deps [--tree] [--installed] [--include-build] [--include-test] \
         [--include-optional] [-n] [--for-each] [--direct] [FORMULA|CASK...]",
    ),
    (
        "desc",
        "fastbrew desc [-s|-n|-d] FORMULA|CASK|TEXT|/REGEX/...",
    ),
    (
        "doctor",
        "fastbrew doctor [--list-checks] [DIAGNOSTIC_CHECK...]",
    ),
    (
        "fetch",
        "fastbrew fetch [--force] [--deps] [--formula|--cask] FORMULA|CASK...",
    ),
    ("help", "fastbrew help [COMMAND]"),
    ("home", "fastbrew home [FORMULA|CASK...]"),
    (
        "info",
        "fastbrew info [--json=v2] [--installed] [--formula|--cask] [FORMULA|CASK...]",
    ),
    (
        "install",
        "fastbrew install [--formula|--cask] [--only-dependencies] [--ignore-dependencies] \
         [--force] [--dry-run] [--overwrite] [--skip-post-install] FORMULA|CASK...",
    ),
    (
        "leaves",
        "fastbrew leaves [--installed-on-request] [--installed-as-dependency]",
    ),
    (
        "link",
        "fastbrew link [--overwrite] [--dry-run] [--force] FORMULA...",
    ),
    (
        "list",
        "fastbrew list [--formula|--cask] [--versions] [--pinned] [--full-name] [-1] [FORMULA|CASK...]",
    ),
    ("missing", "fastbrew missing [--hide=FORMULA] [FORMULA...]"),
    (
        "options",
        "fastbrew options [--compact] [--installed] [FORMULA...]",
    ),
    (
        "outdated",
        "fastbrew outdated [--formula|--cask] [--json=v2] [--greedy] [FORMULA|CASK...]",
    ),
    ("pin", "fastbrew pin FORMULA..."),
    ("postinstall", "fastbrew postinstall FORMULA..."),
    (
        "reinstall",
        "fastbrew reinstall [--formula|--cask] [--force] [--dry-run] FORMULA|CASK...",
    ),
    (
        "search",
        "fastbrew search [--desc] [--formula|--cask] TEXT|/REGEX/",
    ),
    (
        "services",
        "fastbrew services [list|info|start|stop|restart|run|kill|cleanup] [--all] [--json] [FORMULA...]",
    ),
    ("shellenv", "fastbrew shellenv [SHELL]"),
    ("tap", "fastbrew tap [--force] [USER/REPO] [URL]"),
    (
        "tap-info",
        "fastbrew tap-info [--installed] [--json] [TAP...]",
    ),
    (
        "uninstall",
        "fastbrew uninstall [--force] [--ignore-dependencies] [--zap] FORMULA|CASK...",
    ),
    ("unlink", "fastbrew unlink [--dry-run] FORMULA..."),
    ("unpin", "fastbrew unpin FORMULA..."),
    ("untap", "fastbrew untap [--force] TAP..."),
    (
        "update",
        "fastbrew update [--force] [--auto-update] [--quiet]",
    ),
    (
        "upgrade",
        "fastbrew upgrade [--formula|--cask] [--dry-run] [--greedy] [FORMULA|CASK...]",
    ),
    (
        "uses",
        "fastbrew uses [--installed] [--recursive] [--include-build] [--include-test] FORMULA...",
    ),
    (
        "which-formula",
        "fastbrew which-formula [--explain] COMMAND...",
    ),
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

/// External commands: executables in every tap's `cmd/` directory plus
/// `brew-*` on `PATH`, with the `brew-` prefix and any extension removed
/// (`Commands.external_commands`).
pub fn external_commands(cfg: &Config) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |path: &Path| {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            return;
        };
        if !is_executable(path) {
            return;
        }
        names.push(stem.trim_start_matches("brew-").trim().to_string());
    };
    for t in crate::tap::installed_taps(cfg) {
        for file in crate::tap::command_files(cfg, &t) {
            push(&file);
        }
    }
    // Homebrew 6 lists only tap `cmd/` files here, but `brew <cmd>` also runs
    // `brew-<cmd>` from `PATH`, so those are listed too.
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("brew-"))
                {
                    push(&p);
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub fn commands(ctx: &Ctx, args: &CommandsArgs) -> Result<()> {
    let builtin: Vec<String> = BUILTIN_COMMANDS.iter().map(|s| s.to_string()).collect();
    let external = external_commands(&ctx.cfg);

    if args.quiet_list || ctx.quiet {
        let mut names = builtin;
        names.extend(external);
        if args.include_aliases {
            names.extend(super::COMMAND_ALIASES.iter().map(|(a, _)| a.to_string()));
        }
        names.sort();
        names.dedup();
        output::print_columns(&names);
        return Ok(());
    }

    // `next if commands.blank?`: an empty section prints nothing at all, so
    // fastbrew never shows a "Built-in developer commands" header.
    let mut separator = false;
    for (title, names) in [
        ("Built-in commands", &builtin),
        ("Built-in developer commands", &Vec::new()),
        ("External commands", &external),
    ] {
        if names.is_empty() {
            continue;
        }
        if separator {
            println!();
        }
        separator = true;
        output::ohai(title);
        output::print_columns(names);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// completions
// ---------------------------------------------------------------------------

/// `Utils::Link.link_completions`: where each shell's completions are linked.
const COMPLETION_DIRS: [(&str, &str); 3] = [
    ("bash", "etc/bash_completion.d"),
    ("zsh", "share/zsh/site-functions"),
    ("fish", "share/fish/vendor_completions.d"),
];

/// `Homebrew::Completions.link_completions?`, stored the way Homebrew stores
/// it (`git config homebrew.linkcompletions` in `$HOMEBREW_REPOSITORY`) when
/// that repository exists, and in `$CACHE/fastbrew/settings` otherwise.
pub fn link_completions_enabled(cfg: &Config) -> bool {
    if cfg.repository.join(".git/config").is_file() {
        return git_setting(cfg, "homebrew.linkcompletions").as_deref() == Some("true");
    }
    std::fs::read_to_string(settings_file(cfg))
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

fn set_link_completions(cfg: &Config, value: bool) {
    if cfg.repository.join(".git/config").is_file() {
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&cfg.repository)
            .args([
                "config",
                "--replace-all",
                "homebrew.linkcompletions",
                if value { "true" } else { "false" },
            ])
            .output();
        return;
    }
    let file = settings_file(cfg);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, if value { "true" } else { "false" });
}

fn settings_file(cfg: &Config) -> PathBuf {
    cfg.cache_fastbrew().join("settings/linkcompletions")
}

fn git_setting(cfg: &Config, key: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&cfg.repository)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

pub fn completions(ctx: &Ctx, args: &CompletionsArgs) -> Result<()> {
    match args.subcommand.as_deref().unwrap_or("state") {
        "state" => {
            if link_completions_enabled(&ctx.cfg) {
                println!("Completions are linked.");
            } else {
                println!("Completions are not linked.");
            }
        }
        "link" => {
            set_link_completions(&ctx.cfg, true);
            for t in crate::tap::installed_taps(&ctx.cfg) {
                link_tap_completions(&ctx.cfg, &t);
            }
            println!("Completions are now linked.");
        }
        "unlink" => {
            set_link_completions(&ctx.cfg, false);
            for t in crate::tap::installed_taps(&ctx.cfg) {
                unlink_tap_completions(&ctx.cfg, &t);
            }
            println!("Completions are no longer linked.");
        }
        other => {
            return Err(Error::user(format!(
                "Unknown subcommand: {other}\nUsage: fastbrew completions [link|unlink|state]"
            )));
        }
    }
    Ok(())
}

/// `Tap#link_completions_and_manpages`: manpages always, completions only
/// when the user opted in with `fastbrew completions link`.
pub fn link_tap_completions(cfg: &Config, tap: &Tap) {
    let root = tap.path(cfg);
    link_tree(
        &root.join("manpages"),
        &cfg.prefix.join("share/man/man1"),
        "fastbrew tap --repair",
    );
    if link_completions_enabled(cfg) || tap.is_official() {
        for (shell, dest) in COMPLETION_DIRS {
            link_tree(
                &root.join("completions").join(shell),
                &cfg.prefix.join(dest),
                "fastbrew tap --repair",
            );
        }
    } else {
        unlink_completions_only(cfg, tap);
    }
}

pub fn unlink_tap_completions(cfg: &Config, tap: &Tap) {
    unlink_tree(
        &tap.path(cfg).join("manpages"),
        &cfg.prefix.join("share/man/man1"),
    );
    unlink_completions_only(cfg, tap);
}

fn unlink_completions_only(cfg: &Config, tap: &Tap) {
    for (shell, dest) in COMPLETION_DIRS {
        unlink_tree(
            &tap.path(cfg).join("completions").join(shell),
            &cfg.prefix.join(dest),
        );
    }
}

/// `link_src_dst_dirs`: one relative symlink per file, conflicts reported.
fn link_tree(src_dir: &Path, dst_dir: &Path, command: &str) {
    if !src_dir.exists() {
        return;
    }
    let mut conflicts: Vec<PathBuf> = Vec::new();
    for entry in walkdir::WalkDir::new(src_dir).sort_by_file_name() {
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_dir() {
            continue;
        }
        let src = entry.path();
        let Ok(relative) = src.strip_prefix(src_dir) else {
            continue;
        };
        let dst = dst_dir.join(relative);
        if dst.is_symlink() {
            if std::fs::canonicalize(&dst).ok().as_deref() == Some(src) {
                continue;
            }
            let _ = std::fs::remove_file(&dst);
        }
        if dst.exists() {
            conflicts.push(dst);
            continue;
        }
        // `make_relative_symlink` calls `dirname.mkpath` first.
        if let Some(dir) = dst.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = crate::keg::make_relative_symlink(&dst, src);
    }
    if conflicts.is_empty() {
        return;
    }
    let list: Vec<String> = conflicts.iter().map(|p| p.display().to_string()).collect();
    output::onoe(&format!(
        "Could not link:\n{}\n\nPlease delete these paths and run:\n  {command}",
        list.join("\n")
    ));
}

fn unlink_tree(src_dir: &Path, dst_dir: &Path) {
    if !src_dir.exists() {
        return;
    }
    for entry in walkdir::WalkDir::new(src_dir).sort_by_file_name() {
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_dir() {
            continue;
        }
        let src = entry.path();
        let Ok(relative) = src.strip_prefix(src_dir) else {
            continue;
        };
        let dst = dst_dir.join(relative);
        if dst.is_symlink() && std::fs::canonicalize(&dst).ok().as_deref() == Some(src) {
            let _ = std::fs::remove_file(&dst);
        }
    }
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
        let Some((_, usage)) = COMMAND_USAGE.iter().find(|(c, _)| *c == normalized) else {
            return ctx.delegate(&format!("`help {command}` is not implemented by fastbrew"));
        };
        println!("Usage: {usage}");
        if normalized != command {
            println!("\n`{command}` is an alias for `{normalized}`.");
        }
        return Ok(());
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
