//! Command-line interface mirroring Homebrew's command tree.
//!
//! `run(args)` parses arguments, builds `Config`, dispatches to the command
//! implementation, prints errors as `Error: ...` and returns the exit code.
//! Unknown subcommands are delegated to `brew` (see `delegate`).
//!
//! Commands are grouped in submodules by area; each exposes
//! `pub fn run(cfg: &Config, args: &<Args>) -> Result<()>`.

pub mod commands;

use std::ffi::OsString;

pub fn run<I>(_args: I) -> i32
where
    I: IntoIterator<Item = OsString>,
{
    todo!("cli::run")
}
