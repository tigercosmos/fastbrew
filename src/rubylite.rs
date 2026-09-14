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
//! `on_intel` blocks (only the matching branch is kept), and `def install`
//! presence (means a source build is possible). Produces a `FormulaEntry`
//! populated as far as possible; `bottle_checksum` is the bottle for the host
//! tag, `root_url` is returned alongside. Unparseable files return
//! `Err(Error::NeedsDelegation)`.
//!
//! Casks: `version`, `sha256`, `url`, `name`, `desc`, `homepage`, `app`,
//! `binary`, `pkg`, `zap`, `uninstall`, `depends_on`, `auto_updates`.

use std::path::Path;

use crate::error::Result;
use crate::model::{CaskEntry, FormulaEntry};
use crate::platform::BottleTag;

#[derive(Debug, Clone)]
pub struct TapFormula {
    pub entry: FormulaEntry,
    /// `bottle do root_url`, when bottles are declared.
    pub bottle_root_url: Option<String>,
    pub has_install_method: bool,
}

pub fn parse_formula_file(_path: &Path, _tap: &str, _host_tag: &BottleTag) -> Result<TapFormula> {
    todo!("rubylite::parse_formula_file")
}

pub fn parse_cask_file(_path: &Path, _tap: &str) -> Result<CaskEntry> {
    todo!("rubylite::parse_cask_file")
}
