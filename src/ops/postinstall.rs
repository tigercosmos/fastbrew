//! Declarative `post_install_steps` executor (`docs/DESIGN.md` 7), a port of
//! `Library/Homebrew/install_steps.rb`. Also installs `etc`/`var` seed files
//! from the keg into the prefix (`Formula#install_etc_var` with
//! `InstallRenamed` `.default` semantics).

use crate::config::Config;
use crate::error::Result;
use crate::keg::Keg;
use crate::model::FormulaEntry;

pub fn run_post_install(_cfg: &Config, _formula: &FormulaEntry, _keg: &Keg) -> Result<()> {
    todo!("ops::postinstall::run_post_install")
}

pub fn install_etc_var(_cfg: &Config, _keg: &Keg) -> Result<()> {
    todo!("ops::postinstall::install_etc_var")
}
