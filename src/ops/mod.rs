//! High-level operations behind the mutating commands (`docs/DESIGN.md` 6).
//!
//! Each submodule exposes one entry point taking `&Config`, `&Index` and an
//! options struct, printing Homebrew-style progress via `output`, and
//! returning `Result<()>`. They compose `deps`, `bottle`, `keg`, `services`
//! and `cask`.

pub mod caveats;
pub mod cleanup;
pub mod install;
pub mod outdated;
pub mod pin;
pub mod plan;
pub mod postinstall;
pub mod receipt;
pub mod steps;
pub mod uninstall;
pub mod upgrade;
