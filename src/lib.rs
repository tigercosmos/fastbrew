//! fastbrew: a fast Rust reimplementation of the Homebrew client.
//!
//! See `docs/DESIGN.md` for the architecture and `docs/COMPAT.md` for the
//! exact Homebrew formats this crate reproduces.

pub mod api;
pub mod bottle;
pub mod cask;
pub mod cli;
pub mod config;
pub mod delegate;
pub mod deps;
pub mod error;
pub mod keg;
pub mod model;
pub mod ops;
pub mod output;
pub mod platform;
pub mod resolve;
pub mod rubylite;
pub mod services;
pub mod tap;
pub mod update;
pub mod version;

/// The Homebrew version whose behavior and receipt format fastbrew emulates.
/// Written verbatim into `homebrew_version` fields; Homebrew parses it as a
/// version, so it must stay a plain dotted version string.
pub const HOMEBREW_COMPAT_VERSION: &str = "6.0.22";

/// fastbrew's own version, shown by `fastbrew --version`.
pub const FASTBREW_VERSION: &str = env!("CARGO_PKG_VERSION");
