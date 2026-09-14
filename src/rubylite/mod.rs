//! Metadata extraction from Ruby formula and cask files without evaluating Ruby.
//!
//! Handles the common DSL of third-party taps: `desc`, `homepage`, `url`
//! (with `tag:`/`revision:`), `version`, `sha256`, `license`, `revision`,
//! `version_scheme`, `depends_on "x" => :build` (and `[:build, :test]`),
//! `uses_from_macos`, `keg_only`, `conflicts_with`, `caveats` heredocs,
//! `service do ... end` (`run`, `keep_alive`, `working_dir`, `log_path`,
//! `error_log_path`, `run_type`, `environment_variables`), `bottle do ... end`
//! (`root_url`, `rebuild`, `sha256 cellar: :any, arm64_tahoe: "..."` and the
//! legacy `sha256 "..." => :tag` forms), `on_macos`/`on_linux`/`on_arm`/
//! `on_intel` blocks (only the matching branch is kept), plus `if OS.mac?` /
//! `Hardware::CPU.arm?` conditionals, and `def install` presence (means a
//! source build is possible). Produces a `FormulaEntry` populated as far as
//! possible; `bottle_checksum` is the bottle for the host tag, `root_url` is
//! returned alongside. Unparseable files return `Err(Error::NeedsDelegation)`.
//!
//! Casks: `version`, `sha256`, `url`, `name`, `desc`, `homepage`, `app`,
//! `binary`, `pkg`, `zap`, `uninstall`, `depends_on`, `auto_updates`.

mod cask;
mod formula;
mod host;
mod scanner;
mod value;

use std::path::Path;

use crate::error::{Error, Result};
use crate::model::{CaskEntry, FormulaEntry};
use crate::platform::BottleTag;

pub use formula::detect_version_from_url;
pub use host::HostCtx;

#[derive(Debug, Clone)]
pub struct TapFormula {
    pub entry: FormulaEntry,
    /// `bottle do root_url`, when bottles are declared.
    pub bottle_root_url: Option<String>,
    pub has_install_method: bool,
}

/// Formula name from a tap file path.
///
/// Homebrew derives the name from the file, not the class (`Formulary.class_s`
/// maps `python@3.14` to `PythonAT314`, which cannot be inverted reliably).
pub fn name_from_path(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

pub fn parse_formula_file(path: &Path, tap: &str, host_tag: &BottleTag) -> Result<TapFormula> {
    let source = std::fs::read_to_string(path).map_err(|e| Error::NeedsDelegation {
        reason: format!("cannot read {}: {e}", path.display()),
    })?;
    let name = name_from_path(path);
    parse_formula_source(&source, &name, tap, host_tag).map_err(|reason| Error::NeedsDelegation {
        reason: format!("{}: {reason}", path.display()),
    })
}

/// Parse formula text. The `Err` payload explains why delegation is needed.
pub fn parse_formula_source(
    source: &str,
    name: &str,
    tap: &str,
    host_tag: &BottleTag,
) -> std::result::Result<TapFormula, String> {
    let parsed = formula::parse_formula(source, name, tap, host_tag);
    if !parsed.is_formula {
        return Err("no `class ... < Formula` found".to_string());
    }
    if parsed.entry.stable_url_args.is_empty() && parsed.entry.stable_version.is_none() {
        return Err("no url or version could be extracted".to_string());
    }
    if parsed.entry.stable_version.is_none() {
        return Err("no version could be extracted".to_string());
    }
    Ok(TapFormula {
        entry: parsed.entry,
        bottle_root_url: parsed.bottle_root_url,
        has_install_method: parsed.has_install_method,
    })
}

pub fn parse_cask_file(path: &Path, tap: &str) -> Result<CaskEntry> {
    let host_tag = crate::platform::Host::detect().bottle_tag();
    parse_cask_file_for(path, tap, &host_tag)
}

/// `parse_cask_file` with an explicit host tag (tests pin one).
pub fn parse_cask_file_for(path: &Path, tap: &str, host_tag: &BottleTag) -> Result<CaskEntry> {
    let source = std::fs::read_to_string(path).map_err(|e| Error::NeedsDelegation {
        reason: format!("cannot read {}: {e}", path.display()),
    })?;
    let token = name_from_path(path);
    let mut entry = parse_cask_source(&source, &token, tap, host_tag).map_err(|reason| {
        Error::NeedsDelegation {
            reason: format!("{}: {reason}", path.display()),
        }
    })?;
    entry.ruby_source_path = Some(relative_cask_path(path, &token));
    Ok(entry)
}

/// `Casks/<token>.rb` or the sharded `Casks/<first letter>/<token>.rb`.
fn relative_cask_path(path: &Path, token: &str) -> String {
    let mut parts: Vec<String> = vec![];
    for component in path.components().rev() {
        let name = component.as_os_str().to_string_lossy().into_owned();
        parts.push(name.clone());
        if name == "Casks" {
            break;
        }
    }
    parts.reverse();
    if parts.first().map(String::as_str) == Some("Casks") {
        parts.join("/")
    } else {
        format!("Casks/{token}.rb")
    }
}

/// Parse cask text. The `Err` payload explains why delegation is needed.
pub fn parse_cask_source(
    source: &str,
    token: &str,
    tap: &str,
    host_tag: &BottleTag,
) -> std::result::Result<CaskEntry, String> {
    let parsed = cask::parse_cask(source, token, tap, host_tag);
    if !parsed.is_cask {
        return Err("no `cask \"...\" do` block found".to_string());
    }
    if parsed.has_ruby_blocks {
        return Err("cask has preflight/postflight Ruby blocks".to_string());
    }
    if parsed.entry.url_args.is_empty() {
        return Err("no url could be extracted".to_string());
    }
    Ok(parsed.entry)
}

#[cfg(test)]
mod tests;
