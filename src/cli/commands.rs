//! clap definitions for every supported command and their handlers.
//!
//! Naming and flags follow `Library/Homebrew/cmd/*.rb` (see the flag lists in
//! `docs/DESIGN.md` 5). Global flags accepted anywhere: `-v/--verbose`,
//! `-q/--quiet`, `-d/--debug`, `-h/--help`.

use std::ffi::OsString;

use clap::{Args, Parser, Subcommand};

use crate::error::{Error, Result};

use super::{Ctx, misc, mutate, query};

#[derive(Parser, Debug)]
#[command(
    name = "fastbrew",
    about = "A fast Rust reimplementation of the Homebrew client",
    disable_version_flag = true,
    disable_help_subcommand = true,
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    // `-v`, `-q` and `-d` are stripped before clap sees them (see
    // `GlobalFlags::strip`) so that a subcommand's own short of the same
    // letter (`desc -d`) keeps working.
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Display brief statistics for your Homebrew installation, or information about a formula or cask.
    Info(InfoArgs),
    /// Perform a substring search of formula and cask names.
    Search(SearchArgs),
    /// Display a formula's or cask's name and one-line description.
    Desc(DescArgs),
    /// Open a formula's or cask's homepage in a browser.
    Home(HomeArgs),
    /// List installed formulae and casks, or the files in a keg.
    List(ListArgs),
    /// Show dependencies for formulae.
    Deps(DepsArgs),
    /// Show formulae and casks that specify a formula as a dependency.
    Uses(UsesArgs),
    /// List installed formulae that are not dependencies of another installed formula.
    Leaves(LeavesArgs),
    /// List installed casks and formulae that have an updated version available.
    Outdated(OutdatedArgs),
    /// Check the given formula kegs for missing dependencies.
    Missing(MissingArgs),
    /// Show install options specific to a formula.
    Options(OptionsArgs),
    /// Show lists of built-in and external commands.
    Commands(CommandsArgs),
    /// Show Homebrew and system configuration info useful for debugging.
    Config,
    /// Print export statements for the current shell.
    Shellenv(ShellenvArgs),
    /// Show which formula provides a given executable.
    #[command(name = "which-formula")]
    WhichFormula(WhichFormulaArgs),
    /// Fetch the newest version of Homebrew's package metadata.
    Update(UpdateArgs),
    /// Show help for a command.
    Help(HelpArgs),

    // -- mutating commands: parsed here, executed by `ops`/`cask`/`services` --
    /// Install a formula or cask.
    Install(mutate::InstallArgs),
    /// Uninstall and then reinstall a formula or cask.
    Reinstall(mutate::InstallArgs),
    /// Upgrade outdated casks and formulae.
    Upgrade(mutate::UpgradeArgs),
    /// Uninstall a formula or cask.
    Uninstall(mutate::UninstallArgs),
    /// Uninstall formulae that were only installed as a dependency.
    Autoremove(mutate::AutoremoveArgs),
    /// Remove stale lock files and outdated downloads.
    Cleanup(mutate::CleanupArgs),
    /// Symlink all of a formula's installed files into Homebrew's prefix.
    Link(mutate::LinkArgs),
    /// Remove symlinks for a formula from Homebrew's prefix.
    Unlink(mutate::LinkArgs),
    /// Pin the specified formulae, preventing them from being upgraded.
    Pin(mutate::PinArgs),
    /// Unpin formulae, allowing them to be upgraded.
    Unpin(mutate::PinArgs),
    /// Rerun the post-install steps for formulae.
    Postinstall(mutate::PostinstallArgs),
    /// Download a bottle or cask without installing it.
    Fetch(mutate::FetchArgs),
    /// Tap a formula repository.
    Tap(mutate::TapArgs),
    /// Remove a tapped formula repository.
    Untap(mutate::UntapArgs),
    /// Manage background services with macOS' launchctl.
    Services(mutate::ServicesArgs),

    /// Anything fastbrew does not implement is handed to the Ruby `brew`.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

// ---------------------------------------------------------------------------
// Query command arguments
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct InfoArgs {
    #[arg(value_name = "formula|cask")]
    pub names: Vec<String>,
    /// Print a JSON representation (`--json=v1` or `--json=v2`).
    #[arg(long, num_args = 0..=1, default_missing_value = "v1", value_name = "version")]
    pub json: Option<String>,
    /// Print information about all installed formulae and casks.
    #[arg(long)]
    pub installed: bool,
    /// Treat all named arguments as formulae.
    #[arg(long, visible_alias = "formulae")]
    pub formula: bool,
    /// Treat all named arguments as casks.
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
    /// Open the GitHub source page in a browser.
    #[arg(long)]
    pub github: bool,
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    #[arg(value_name = "text|/regex/")]
    pub query: Vec<String>,
    /// Search for formulae and casks with a description matching the query.
    #[arg(long)]
    pub desc: bool,
    /// Search only formulae.
    #[arg(long, visible_alias = "formulae")]
    pub formula: bool,
    /// Search only casks.
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
}

#[derive(Args, Debug)]
pub struct DescArgs {
    #[arg(value_name = "formula|cask|text|/regex/")]
    pub names: Vec<String>,
    /// Search names and descriptions.
    #[arg(short = 's', long)]
    pub search: bool,
    /// Search names only.
    #[arg(short = 'n', long)]
    pub name: bool,
    /// Search descriptions only.
    #[arg(short = 'd', long)]
    pub description: bool,
    #[arg(long, visible_alias = "formulae")]
    pub formula: bool,
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
}

#[derive(Args, Debug)]
pub struct HomeArgs {
    #[arg(value_name = "formula|cask")]
    pub names: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    #[arg(value_name = "formula|cask")]
    pub names: Vec<String>,
    /// Show the version number for installed formulae, or only the specified
    /// formulae if they are installed.
    #[arg(long)]
    pub versions: bool,
    /// Only show formulae with multiple installed versions.
    #[arg(long)]
    pub multiple: bool,
    /// List only pinned formulae.
    #[arg(long)]
    pub pinned: bool,
    #[arg(long, visible_alias = "formulae")]
    pub formula: bool,
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
    /// Print formulae with fully-qualified names.
    #[arg(long)]
    pub full_name: bool,
    /// Force output to be one entry per line.
    #[arg(short = '1')]
    pub one: bool,
    /// List formulae installed on request.
    #[arg(long)]
    pub installed_on_request: bool,
    /// List formulae installed as dependencies.
    #[arg(long, visible_alias = "no-installed-on-request")]
    pub installed_as_dependency: bool,
    /// Print a JSON representation (requires `--versions`).
    #[arg(long)]
    pub json: bool,
    /// Passed through to `ls`: long format.
    #[arg(short = 'l')]
    pub long: bool,
    /// Passed through to `ls`: reverse order.
    #[arg(short = 'r')]
    pub reverse: bool,
    /// Passed through to `ls`: sort by modification time.
    #[arg(short = 't')]
    pub by_time: bool,
}

#[derive(Args, Debug)]
pub struct DepsArgs {
    #[arg(value_name = "formula|cask")]
    pub names: Vec<String>,
    /// Show dependencies as a tree.
    #[arg(long)]
    pub tree: bool,
    /// List dependencies in topological order.
    #[arg(short = 'n', long)]
    pub topological: bool,
    /// List only the direct dependencies declared in the formula.
    #[arg(short = '1', long, visible_aliases = ["declared", "1"])]
    pub direct: bool,
    /// Show the union of dependencies for multiple formulae.
    #[arg(long)]
    pub union: bool,
    /// List dependencies separately for each formula.
    #[arg(long)]
    pub for_each: bool,
    /// List dependencies of all installed formulae.
    #[arg(long)]
    pub installed: bool,
    #[arg(long)]
    pub include_build: bool,
    #[arg(long)]
    pub include_test: bool,
    #[arg(long)]
    pub include_optional: bool,
    #[arg(long)]
    pub include_implicit: bool,
    #[arg(long)]
    pub skip_recommended: bool,
    /// List dependencies that are not currently installed.
    #[arg(long)]
    pub missing: bool,
    /// Print dependencies with fully-qualified names.
    #[arg(long)]
    pub full_name: bool,
    /// Mark build, test, implicit, optional and recommended dependencies.
    #[arg(long)]
    pub annotate: bool,
    #[arg(long, visible_alias = "formulae")]
    pub formula: bool,
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
}

#[derive(Args, Debug)]
pub struct UsesArgs {
    #[arg(value_name = "formula", required = true)]
    pub names: Vec<String>,
    /// Only list installed formulae.
    #[arg(long)]
    pub installed: bool,
    /// Include all levels of dependencies.
    #[arg(long)]
    pub recursive: bool,
    #[arg(long)]
    pub include_build: bool,
    #[arg(long)]
    pub include_test: bool,
    #[arg(long)]
    pub include_optional: bool,
    #[arg(long)]
    pub skip_recommended: bool,
    #[arg(long, visible_alias = "formulae")]
    pub formula: bool,
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
}

#[derive(Args, Debug)]
pub struct LeavesArgs {
    /// Only list leaves that were manually installed.
    #[arg(short = 'r', long)]
    pub installed_on_request: bool,
    /// Only list leaves that were installed as dependencies.
    #[arg(short = 'p', long)]
    pub installed_as_dependency: bool,
}

#[derive(Args, Debug)]
pub struct OutdatedArgs {
    #[arg(value_name = "formula|cask")]
    pub names: Vec<String>,
    #[arg(long, visible_alias = "formulae")]
    pub formula: bool,
    #[arg(long, visible_alias = "casks")]
    pub cask: bool,
    /// Print a JSON representation.
    #[arg(long, num_args = 0..=1, default_missing_value = "v2", value_name = "version")]
    pub json: Option<String>,
    /// Also include casks with `auto_updates true` or `version :latest`.
    #[arg(short = 'g', long)]
    pub greedy: bool,
    #[arg(long)]
    pub greedy_latest: bool,
    #[arg(long)]
    pub greedy_auto_updates: bool,
}

#[derive(Args, Debug)]
pub struct MissingArgs {
    #[arg(value_name = "formula")]
    pub names: Vec<String>,
    /// Act as if none of the specified formulae are installed.
    #[arg(long, value_delimiter = ',')]
    pub hide: Vec<String>,
}

#[derive(Args, Debug)]
pub struct OptionsArgs {
    #[arg(value_name = "formula")]
    pub names: Vec<String>,
    /// Show options on a single line separated by spaces.
    #[arg(long)]
    pub compact: bool,
    /// Show options for installed formulae.
    #[arg(long)]
    pub installed: bool,
}

#[derive(Args, Debug)]
pub struct CommandsArgs {
    /// List only the names of commands without category headers.
    #[arg(short = 'q', long)]
    pub quiet_list: bool,
    /// Include aliases of internal commands.
    #[arg(long)]
    pub include_aliases: bool,
}

#[derive(Args, Debug)]
pub struct ShellenvArgs {
    #[arg(value_name = "shell")]
    pub shell: Option<String>,
}

#[derive(Args, Debug)]
pub struct WhichFormulaArgs {
    #[arg(value_name = "command", required = true)]
    pub commands: Vec<String>,
    /// Print a message explaining how to install the command.
    #[arg(long)]
    pub explain: bool,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Always do a slower, full update check.
    #[arg(short = 'f', long)]
    pub force: bool,
    /// Run in the background as part of another command.
    #[arg(long)]
    pub auto_update: bool,
    /// Print a verbose report.
    #[arg(long)]
    pub preinstall: bool,
}

#[derive(Args, Debug)]
pub struct HelpArgs {
    #[arg(value_name = "command")]
    pub command: Option<String>,
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub fn dispatch(ctx: &Ctx, argv: &[OsString]) -> Result<()> {
    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(e) => {
            // clap's own help/version output is fine to show as-is.
            use clap::error::ErrorKind;
            if matches!(
                e.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                print!("{e}");
                return Ok(());
            }
            return Err(Error::user(
                e.to_string()
                    .lines()
                    .next()
                    .unwrap_or("invalid usage")
                    .trim_start_matches("error: ")
                    .to_string(),
            ));
        }
    };

    match &cli.command {
        Cmd::Info(a) => query::info(ctx, a),
        Cmd::Search(a) => query::search(ctx, a),
        Cmd::Desc(a) => query::desc(ctx, a),
        Cmd::Home(a) => query::home(ctx, a),
        Cmd::List(a) => query::list(ctx, a),
        Cmd::Deps(a) => query::deps(ctx, a),
        Cmd::Uses(a) => query::uses(ctx, a),
        Cmd::Leaves(a) => query::leaves(ctx, a),
        Cmd::Outdated(a) => query::outdated(ctx, a),
        Cmd::Missing(a) => query::missing(ctx, a),
        Cmd::Options(a) => query::options(ctx, a),
        Cmd::WhichFormula(a) => query::which_formula(ctx, a),
        Cmd::Commands(a) => misc::commands(ctx, a),
        Cmd::Config => misc::config(ctx),
        Cmd::Shellenv(a) => misc::shellenv(ctx, a),
        Cmd::Help(a) => misc::help(ctx, a),
        Cmd::Update(a) => misc::update(ctx, a),

        Cmd::Install(a) => mutate::install(ctx, a, false),
        Cmd::Reinstall(a) => mutate::install(ctx, a, true),
        Cmd::Upgrade(a) => mutate::upgrade(ctx, a),
        Cmd::Uninstall(a) => mutate::uninstall(ctx, a),
        Cmd::Autoremove(a) => mutate::autoremove(ctx, a),
        Cmd::Cleanup(a) => mutate::cleanup(ctx, a),
        Cmd::Link(a) => mutate::link(ctx, a, true),
        Cmd::Unlink(a) => mutate::link(ctx, a, false),
        Cmd::Pin(a) => mutate::pin(ctx, a, true),
        Cmd::Unpin(a) => mutate::pin(ctx, a, false),
        Cmd::Postinstall(a) => mutate::postinstall(ctx, a),
        Cmd::Fetch(a) => mutate::fetch(ctx, a),
        Cmd::Tap(a) => mutate::tap(ctx, a),
        Cmd::Untap(a) => mutate::untap(ctx, a),
        Cmd::Services(a) => mutate::services(ctx, a),

        Cmd::External(args) => {
            let name = args
                .first()
                .map(|a| a.to_string_lossy().into_owned())
                .unwrap_or_default();
            ctx.delegate(&format!("`{name}` is not implemented by fastbrew"))
        }
    }
}
