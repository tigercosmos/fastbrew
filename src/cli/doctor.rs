//! `doctor`: a native subset of `Library/Homebrew/diagnostic.rb`.
//!
//! Only the checks that need nothing but the prefix are native; everything
//! else is left to the Ruby `brew doctor`, which fastbrew runs when the user
//! asks for a check it does not implement. The message text, the
//! `Please note that these warnings...` preamble, the `Your system is ready
//! to brew.` line and the non-zero exit on findings follow `cmd/doctor.rb`.

use clap::Args;

use crate::error::Result;
use crate::keg;
use crate::output;

use super::Ctx;

/// The checks fastbrew runs itself, in `Checks#all` order (sorted, as Ruby's
/// `methods.grep(/^check_/).sort` produces).
pub const NATIVE_CHECKS: &[&str] = &[
    "check_for_broken_symlinks",
    "check_for_unlinked_but_not_keg_only",
    "check_missing_opt_links",
];

#[derive(Args, Debug)]
pub struct DoctorArgs {
    #[arg(value_name = "diagnostic_check")]
    pub checks: Vec<String>,
    /// List all audit methods, which can be run individually if provided as arguments.
    #[arg(long)]
    pub list_checks: bool,
    /// Enable debugging and profiling of audit methods.
    #[arg(short = 'D', long)]
    pub audit_debug: bool,
}

pub fn doctor(ctx: &Ctx, args: &DoctorArgs) -> Result<()> {
    if args.list_checks {
        for check in NATIVE_CHECKS {
            println!("{check}");
        }
        return Ok(());
    }
    // A named check fastbrew does not implement is the Ruby's business.
    if let Some(unknown) = args
        .checks
        .iter()
        .find(|c| !NATIVE_CHECKS.contains(&c.as_str()))
    {
        return ctx.delegate(&format!("`doctor {unknown}` needs Homebrew's diagnostics"));
    }
    let wanted: Vec<&str> = if args.checks.is_empty() {
        NATIVE_CHECKS.to_vec()
    } else {
        args.checks.iter().map(String::as_str).collect()
    };

    let mut first = true;
    for check in wanted {
        let Some(finding) = run_check(ctx, check) else {
            continue;
        };
        output::set_failed();
        if first && !ctx.quiet {
            eprintln!(
                "{}",
                output::bold(
                    "Please note that these warnings are just used to help the Homebrew maintainers\n\
                     with debugging if you file an issue. If everything you use Homebrew for is\n\
                     working fine: please don't worry or file an issue; just ignore this. Thanks!"
                )
            );
        }
        first = false;
        eprintln!();
        output::opoo(&finding);
    }

    if ctx.quiet {
        return Ok(());
    }
    if output::is_failed() {
        println!(
            "This is a fastbrew-specific subset of `brew doctor`; run `brew doctor` for the full set of checks."
        );
    } else {
        println!("Your system is ready to brew.");
    }
    Ok(())
}

fn run_check(ctx: &Ctx, check: &str) -> Option<String> {
    match check {
        "check_for_broken_symlinks" => check_for_broken_symlinks(ctx),
        "check_for_unlinked_but_not_keg_only" => check_for_unlinked_but_not_keg_only(ctx),
        "check_missing_opt_links" => check_missing_opt_links(ctx),
        _ => None,
    }
}

/// `append_indented_list`: two spaces per entry, one per line.
fn indented(items: &[String]) -> String {
    items
        .iter()
        .map(|i| format!("  {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Port of `Diagnostic::Checks#check_for_broken_symlinks`.
fn check_for_broken_symlinks(ctx: &Ctx) -> Option<String> {
    let mut broken: Vec<String> = Vec::new();
    for dir in keg::link::KEG_LINK_DIRECTORIES
        .iter()
        .chain(["opt", "var/homebrew/linked"].iter())
    {
        let root = ctx.cfg.prefix.join(dir);
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&root)
            .follow_links(false)
            .sort_by_file_name()
        {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.is_symlink() && std::fs::metadata(path).is_err() {
                broken.push(path.display().to_string());
            }
        }
    }
    broken.sort();
    broken.dedup();
    if broken.is_empty() {
        return None;
    }
    Some(format!(
        "Broken symlinks were found:\n{}\n\nRemove them with `fastbrew cleanup`",
        indented(&broken)
    ))
}

/// Port of `check_for_unlinked_but_not_keg_only`.
fn check_for_unlinked_but_not_keg_only(ctx: &Ctx) -> Option<String> {
    let index = ctx.index().ok();
    let unlinked: Vec<String> = keg::installed_formula_names(&ctx.cfg)
        .into_iter()
        .filter(|name| !ctx.cfg.linked_record(name).is_dir())
        .filter(|name| {
            // A keg-only formula is meant to stay unlinked.
            index
                .and_then(|i| i.formula(name))
                .map(|f| !f.is_keg_only())
                .unwrap_or(true)
        })
        .collect();
    if unlinked.is_empty() {
        return None;
    }
    let commands: Vec<String> = unlinked.clone();
    Some(format!(
        "You have unlinked kegs in your Cellar.\nLeaving kegs unlinked can lead to build-trouble and cause formulae that depend on\nthose kegs to fail to run properly once built.\n\nRun `fastbrew link` on these:\n{}",
        indented(&commands)
    ))
}

/// fastbrew-specific: an installed keg with no `opt/<name>` record breaks
/// every dependent's dylib paths, which Homebrew only notices at link time.
fn check_missing_opt_links(ctx: &Ctx) -> Option<String> {
    let missing: Vec<String> = keg::installed_formula_names(&ctx.cfg)
        .into_iter()
        .filter(|name| !ctx.cfg.opt_record(name).exists())
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "Some installed kegs have no `opt` link:\n{}\n\nRun `fastbrew link` on these to recreate it.",
        indented(&missing)
    ))
}
