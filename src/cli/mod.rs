//! Command-line interface mirroring Homebrew's command tree.
//!
//! `run(args)` parses arguments, builds `Config`, dispatches to the command
//! implementation, prints errors as `Error: ...` and returns the exit code.
//! Unknown subcommands are delegated to `brew` (see `delegate`).
//!
//! Commands are grouped in submodules by area; each exposes
//! `pub fn run(cfg: &Config, args: &<Args>) -> Result<()>`.

pub mod commands;
pub mod doctor;
pub mod fmt;
pub mod misc;
pub mod mutate;
pub mod query;
pub mod services;
pub mod taps;

use std::cell::OnceCell;
use std::ffi::OsString;

use crate::api::index::Index;
use crate::api::taps::TapIndex;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::output;
use crate::platform::BottleTag;

/// `Commands::HOMEBREW_INTERNAL_COMMAND_ALIASES` plus `dj` (a fastbrew
/// shorthand for `doctor`, which delegates).
pub const COMMAND_ALIASES: &[(&str, &str)] = &[
    ("ls", "list"),
    ("homepage", "home"),
    ("-S", "search"),
    ("up", "update"),
    ("ln", "link"),
    ("instal", "install"),
    ("uninstal", "uninstall"),
    ("post_install", "postinstall"),
    ("rm", "uninstall"),
    ("remove", "uninstall"),
    ("abv", "info"),
    ("dr", "doctor"),
    ("dj", "doctor"),
    ("--repo", "--repository"),
    ("environment", "--env"),
    ("--config", "config"),
    ("-v", "--version"),
    ("lc", "livecheck"),
    ("tc", "typecheck"),
    ("x", "exec"),
];

pub fn resolve_alias(command: &str) -> &str {
    COMMAND_ALIASES
        .iter()
        .find(|(from, _)| *from == command)
        .map(|(_, to)| *to)
        .unwrap_or(command)
}

/// Everything a command handler needs: configuration, the host bottle tag and
/// a lazily loaded index.
pub struct Ctx {
    pub cfg: Config,
    pub tag: BottleTag,
    pub verbose: bool,
    pub quiet: bool,
    pub debug: bool,
    /// Raw argv, for delegation.
    pub argv: Vec<OsString>,
    index: OnceCell<Index>,
    taps: OnceCell<TapIndex>,
}

impl Ctx {
    pub fn index(&self) -> Result<&Index> {
        if let Some(i) = self.index.get() {
            return Ok(i);
        }
        let index = Index::load(&self.cfg, &self.tag)?;
        let _ = self.index.set(index);
        Ok(self.index.get().expect("index just set"))
    }

    /// Metadata of every installed third-party tap, loaded once per run.
    ///
    /// Only commands that list or scan everything need this; single-name
    /// lookups go through `resolve`, which reaches for a tap only when the
    /// API has no such name.
    pub fn taps(&self) -> &TapIndex {
        self.taps
            .get_or_init(|| TapIndex::load(&self.cfg, &self.tag))
    }

    /// `exec` the Ruby `brew` with this invocation's arguments.
    pub fn delegate(&self, reason: &str) -> Result<()> {
        let args: Vec<OsString> = self.argv.iter().skip(1).cloned().collect();
        crate::delegate::exec_brew(&self.cfg, &args, reason, self.quiet)?;
        Ok(())
    }
}

pub fn run<I>(args: I) -> i32
where
    I: IntoIterator<Item = OsString>,
{
    // Behave like a Unix filter: die quietly when a pager or `head` closes
    // the pipe instead of panicking in `println!`.
    // SAFETY: resetting a signal disposition to the default is always sound.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    let argv: Vec<OsString> = args.into_iter().collect();
    match dispatch(argv) {
        Ok(code) => code,
        Err(Error::NeedsDelegation { reason }) => {
            output::onoe(&reason);
            1
        }
        Err(e) => {
            output::onoe(&e.to_string());
            1
        }
    }
}

fn dispatch(argv: Vec<OsString>) -> Result<i32> {
    let cfg = Config::from_env()?;
    let tag = crate::platform::Host::detect().bottle_tag();

    // Homebrew resolves command aliases before parsing (`brew.sh`).
    let mut normalized = argv.clone();
    if let Some(first) = normalized.get_mut(1)
        && let Some(s) = first.to_str()
    {
        let resolved = resolve_alias(s);
        if resolved != s {
            *first = OsString::from(resolved);
        }
    }

    let global = GlobalFlags::scan(&normalized);
    let normalized = GlobalFlags::strip(normalized);
    output::set_quiet(global.quiet);
    let ctx = Ctx {
        verbose: global.verbose || cfg.verbose,
        quiet: global.quiet,
        debug: global.debug || cfg.debug,
        cfg,
        tag,
        argv,
        index: OnceCell::new(),
        taps: OnceCell::new(),
    };

    // Pseudo-commands whose names start with `--` are handled before clap,
    // which would otherwise read them as flags of the root command.
    if let Some(name) = normalized.get(1).and_then(|a| a.to_str())
        && name.starts_with("--")
        && name != "--help"
    {
        if misc::is_path_command(name) {
            let rest: Vec<String> = normalized[2..]
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            misc::run_path_command(&ctx, name, &rest)?;
            return Ok(i32::from(output::is_failed()));
        }
        // `--env` and the rest of Homebrew's `--`-commands are the Ruby's.
        ctx.delegate(&format!("`{name}` is not implemented by fastbrew"))?;
        return Ok(1);
    }

    commands::dispatch(&ctx, &normalized)?;
    // `Homebrew.failed?`: commands that reported a problem without aborting.
    Ok(i32::from(output::is_failed()))
}

/// Global flags Homebrew accepts anywhere on the command line.
struct GlobalFlags {
    verbose: bool,
    quiet: bool,
    debug: bool,
}

impl GlobalFlags {
    /// Remove the flags handled here so that clap never sees them; a
    /// subcommand's own short of the same name (`desc -d`) is preserved.
    fn strip(argv: Vec<OsString>) -> Vec<OsString> {
        let command = argv.get(1).and_then(|a| a.to_str()).unwrap_or("");
        let keep_short_d = command == "desc";
        let mut out = Vec::with_capacity(argv.len());
        for (i, a) in argv.into_iter().enumerate() {
            if i < 2 {
                out.push(a);
                continue;
            }
            match a.to_str() {
                Some("-v") | Some("--verbose") | Some("-q") | Some("--quiet") | Some("--debug") => {
                    continue;
                }
                Some("-d") if !keep_short_d => continue,
                _ => out.push(a),
            }
        }
        out
    }

    fn scan(argv: &[OsString]) -> Self {
        let mut f = GlobalFlags {
            verbose: false,
            quiet: false,
            debug: false,
        };
        for a in argv.iter().skip(1) {
            match a.to_str() {
                Some("-v") | Some("--verbose") => f.verbose = true,
                Some("-q") | Some("--quiet") => f.quiet = true,
                Some("-d") | Some("--debug") => f.debug = true,
                _ => {}
            }
        }
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve() {
        assert_eq!(resolve_alias("ls"), "list");
        assert_eq!(resolve_alias("rm"), "uninstall");
        assert_eq!(resolve_alias("remove"), "uninstall");
        assert_eq!(resolve_alias("abv"), "info");
        assert_eq!(resolve_alias("up"), "update");
        assert_eq!(resolve_alias("-S"), "search");
        assert_eq!(resolve_alias("homepage"), "home");
        assert_eq!(resolve_alias("ln"), "link");
        assert_eq!(resolve_alias("-v"), "--version");
        assert_eq!(resolve_alias("info"), "info");
    }
}
