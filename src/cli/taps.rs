//! `tap`, `untap` and `tap-info`.
//!
//! Ports of `cmd/tap.rb`, `cmd/untap.rb` and `cmd/tap-info.rb`. The tap
//! operations themselves live in `crate::tap`; this file is the output.

use clap::Args;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::output;
use crate::tap::{self, Tap};

use super::fmt;
use super::{Ctx, misc};

/// `print_section`'s cut-off: beyond this a tap only lists what is installed.
const LISTING_LIMIT: usize = 30;

#[derive(Args, Debug)]
pub struct TapArgs {
    #[arg(value_name = "user/repo")]
    pub name: Option<String>,
    #[arg(value_name = "URL")]
    pub url: Option<String>,
    /// Install or change a tap with a custom remote. Useful for mirrors.
    #[arg(long)]
    pub custom_remote: bool,
    /// Force tapping the core taps even under API mode.
    #[arg(short = 'f', long)]
    pub force: bool,
    /// Add missing symlinks to tap completions and manpages.
    #[arg(long)]
    pub repair: bool,
}

pub fn tap(ctx: &Ctx, args: &TapArgs) -> Result<()> {
    if args.repair {
        for t in tap::installed_taps(&ctx.cfg) {
            misc::link_tap_completions(&ctx.cfg, &t);
        }
        return Ok(());
    }
    let Some(name) = &args.name else {
        for t in tap::installed_taps(&ctx.cfg) {
            println!("{}", t.name());
        }
        return Ok(());
    };
    // `setup-auto-update`: `brew tap` auto-updates only when given a tap.
    crate::update::auto_update_if_needed(&ctx.cfg, "tap", std::slice::from_ref(name));
    match tap::tap_with_outcome(&ctx.cfg, name, args.url.as_deref(), args.force, ctx.quiet) {
        // `cmd/tap.rb` rescues `TapAlreadyTappedError` and exits 0.
        Ok(_) => Ok(()),
        Err(e) if e.to_string().ends_with("already tapped.\n") => Ok(()),
        Err(e) => Err(e),
    }
    .inspect(|()| {
        if let Some(t) = Tap::parse(name) {
            misc::link_tap_completions(&ctx.cfg, &t);
        }
    })
}

#[derive(Args, Debug)]
pub struct UntapArgs {
    #[arg(value_name = "tap", required = true)]
    pub names: Vec<String>,
    /// Untap even if formulae or casks from the tap are installed.
    #[arg(short = 'f', long)]
    pub force: bool,
}

pub fn untap(ctx: &Ctx, args: &UntapArgs) -> Result<()> {
    for name in &args.names {
        if let Some(t) = Tap::parse(name) {
            misc::unlink_tap_completions(&ctx.cfg, &t);
            let _ = std::fs::remove_file(crate::api::taps::cache_path(&ctx.cfg, &t));
        }
        tap::untap(&ctx.cfg, name, args.force)?;
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct TapInfoArgs {
    #[arg(value_name = "tap")]
    pub names: Vec<String>,
    /// Show information on each installed tap.
    #[arg(long)]
    pub installed: bool,
    /// Print a JSON representation (`--json` or `--json=v1`).
    #[arg(long, num_args = 0..=1, require_equals = true,
          default_missing_value = "v1", value_name = "version")]
    pub json: Option<String>,
}

pub fn tap_info(ctx: &Ctx, args: &TapInfoArgs) -> Result<()> {
    if let Some(version) = &args.json
        && version != "v1"
    {
        return Err(Error::user(format!("invalid JSON version: {version}")));
    }
    let taps: Vec<Tap> = if args.installed {
        tap::installed_taps(&ctx.cfg)
    } else {
        let mut named = Vec::new();
        for n in &args.names {
            named.push(
                Tap::parse(n).ok_or_else(|| Error::user(format!("Invalid tap name: '{n}'")))?,
            );
        }
        named.sort_by_key(Tap::name);
        named
    };

    if args.json.is_some() {
        let rows: Vec<Value> = taps.iter().map(|t| tap_json(ctx, t)).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
        return Ok(());
    }
    if taps.is_empty() {
        print_statistics(ctx);
        return Ok(());
    }
    for (i, t) in taps.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_tap_info(ctx, t);
    }
    Ok(())
}

/// `brew tap-info` with no arguments: one summary line for every tap.
fn print_statistics(ctx: &Ctx) {
    let cfg = &ctx.cfg;
    let taps = tap::installed_taps(cfg);
    let formulae: usize = taps.iter().map(|t| tap::formula_files(cfg, t).len()).sum();
    let commands: usize = taps.iter().map(|t| tap::command_files(cfg, t).len()).sum();
    let mut info = format!(
        "{}, 0 private, {}, {}",
        output::plural(taps.len() as u64, "tap"),
        pluralize_formula(formulae),
        output::plural(commands as u64, "command"),
    );
    if cfg.taps_dir().is_dir() {
        let (files, bytes) = crate::keg::disk_usage(&cfg.taps_dir());
        info.push_str(&format!(", {}", fmt::abv(files, bytes)));
    }
    println!("{info}");
}

/// `Utils.pluralize("formula", n)`: the plural is `formulae`.
fn pluralize_formula(n: usize) -> String {
    if n == 1 {
        "1 formula".to_string()
    } else {
        format!("{n} formulae")
    }
}

fn print_tap_info(ctx: &Ctx, t: &Tap) {
    let cfg = &ctx.cfg;
    if !t.is_installed(cfg) {
        println!("{}: Not installed", t.name());
        output::set_failed();
        return;
    }
    let mut info = format!("{}: Installed", t.name());
    let contents = tap::contents(cfg, t);
    if contents.is_empty() {
        info.push_str("\nNo commands/casks/formulae");
    } else {
        info.push_str(&format!("\n{}", contents.join(", ")));
    }
    let (files, bytes) = crate::keg::disk_usage(&t.path(cfg));
    info.push_str(&format!(
        "\n{} ({})",
        t.path(cfg).display(),
        fmt::abv(files, bytes)
    ));
    let remote = t.remote(cfg);
    info.push_str(&format!(
        "\nFrom: {}",
        remote.clone().unwrap_or_else(|| "N/A".to_string())
    ));
    if let Some(r) = &remote
        && *r != t.default_remote()
    {
        info.push_str(&format!("\norigin: {r}"));
    }
    info.push_str(&format!(
        "\nHEAD: {}",
        tap::tap_git_head(cfg, t).unwrap_or_else(|| "(none)".to_string())
    ));
    info.push_str(&format!(
        "\nlast commit: {}",
        tap::git_last_commit(cfg, t).unwrap_or_else(|| "never".to_string())
    ));
    let branch = tap::git_branch(cfg, t);
    if !matches!(branch.as_deref(), Some("main") | Some("master")) {
        info.push_str(&format!(
            "\nbranch: {}",
            branch.unwrap_or_else(|| "(none)".to_string())
        ));
    }
    println!("{info}");
    print_tap_listings(ctx, t);
}

fn print_tap_listings(ctx: &Ctx, t: &Tap) {
    let cfg = &ctx.cfg;
    let commands: Vec<String> = tap::command_files(cfg, t)
        .iter()
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()))
        .map(|n| n.trim_start_matches("brew-").to_string())
        .collect();
    if !commands.is_empty() {
        output::ohai("Commands");
        println!("{}", commands.join(", "));
    }

    let formula_names: Vec<String> = tap::formula_files(cfg, t)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    let cask_tokens: Vec<String> = tap::cask_files(cfg, t)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    let installed_formulae: Vec<String> = formula_names
        .iter()
        .filter(|n| !crate::keg::installed_kegs(cfg, n).is_empty())
        .cloned()
        .collect();
    let installed_casks: Vec<String> = cask_tokens
        .iter()
        .filter(|n| cfg.caskroom().join(n).is_dir())
        .cloned()
        .collect();

    print_section(
        ctx,
        t,
        "Formulae",
        &formula_names,
        &installed_formulae,
        false,
    );
    print_section(ctx, t, "Casks", &cask_tokens, &installed_casks, true);
}

fn print_section(
    ctx: &Ctx,
    t: &Tap,
    label: &str,
    all: &[String],
    installed: &[String],
    cask: bool,
) {
    if all.is_empty() {
        return;
    }
    let decorate = |names: &[String]| -> Vec<String> {
        names
            .iter()
            .map(|n| {
                let is_installed = if cask {
                    ctx.cfg.caskroom().join(n).is_dir()
                } else {
                    !crate::keg::installed_kegs(&ctx.cfg, n).is_empty()
                };
                fmt::install_status(n, is_installed, false)
            })
            .collect()
    };
    if all.len() <= LISTING_LIMIT {
        output::ohai(label);
        output::print_columns(&decorate(all));
    } else if !installed.is_empty() {
        output::ohai(label);
        output::opoo(&format!(
            "Tap has more than {LISTING_LIMIT} {}; showing only installed entries.",
            label.to_lowercase()
        ));
        output::print_columns(&decorate(installed));
    } else {
        output::ohai(label);
        output::opoo(&format!(
            "Tap has more than {LISTING_LIMIT} {} and none are installed.",
            label.to_lowercase()
        ));
        if let Some(remote) = t.remote(&ctx.cfg) {
            println!("See: {remote}");
        }
    }
}

/// `Tap#to_hash`. `private` needs the GitHub API, which fastbrew never calls
/// for a read-only command, so it is reported as `false`.
fn tap_json(ctx: &Ctx, t: &Tap) -> Value {
    let cfg = &ctx.cfg;
    let installed = t.is_installed(cfg);
    let full = |names: Vec<(String, std::path::PathBuf)>| -> Vec<String> {
        names
            .into_iter()
            .map(|(n, _)| t.full_package_name(&n))
            .collect()
    };
    let mut doc = json!({
        "name": t.name(),
        "user": t.user.to_lowercase(),
        "repo": t.repo.clone(),
        "repository": t.repo.clone(),
        "path": t.path(cfg).to_string_lossy(),
        "installed": installed,
        "official": t.is_official(),
        "trusted": true,
        "formula_names": full(tap::formula_files(cfg, t)),
        "cask_tokens": full(tap::cask_files(cfg, t)),
    });
    if !installed {
        return doc;
    }
    let map = doc.as_object_mut().expect("object");
    let paths = |files: Vec<(String, std::path::PathBuf)>| -> Vec<String> {
        files
            .into_iter()
            .map(|(_, p)| p.to_string_lossy().into_owned())
            .collect()
    };
    map.insert(
        "formula_files".into(),
        json!(paths(tap::formula_files(cfg, t))),
    );
    map.insert("cask_files".into(), json!(paths(tap::cask_files(cfg, t))));
    map.insert(
        "command_files".into(),
        json!(
            tap::command_files(cfg, t)
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        ),
    );
    let remote = t.remote(cfg);
    map.insert("remote".into(), json!(remote));
    map.insert(
        "custom_remote".into(),
        json!(remote.as_deref() != Some(t.default_remote().as_str())),
    );
    map.insert("private".into(), json!(false));
    map.insert(
        "HEAD".into(),
        json!(tap::tap_git_head(cfg, t).unwrap_or_else(|| "(none)".to_string())),
    );
    map.insert(
        "last_commit".into(),
        json!(tap::git_last_commit(cfg, t).unwrap_or_else(|| "never".to_string())),
    );
    map.insert(
        "branch".into(),
        json!(tap::git_branch(cfg, t).unwrap_or_else(|| "(none)".to_string())),
    );
    doc
}
