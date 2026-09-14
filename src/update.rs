//! `update` and the auto-update policy (`docs/DESIGN.md` 9).

use crate::config::Config;
use crate::error::Result;

#[derive(Debug, Clone, Default)]
pub struct UpdateReport {
    pub api_updated: bool,
    pub taps_updated: Vec<String>,
    pub new_formulae: Vec<String>,
    pub updated_formulae: Vec<String>,
    pub renamed_formulae: Vec<(String, String)>,
    pub deleted_formulae: Vec<String>,
    pub new_casks: Vec<String>,
    pub updated_casks: Vec<String>,
    pub deleted_casks: Vec<String>,
    pub outdated_formulae: Vec<String>,
    pub outdated_casks: Vec<String>,
}

pub fn update(_cfg: &Config, _force: bool, _quiet: bool, _auto: bool) -> Result<UpdateReport> {
    todo!("update::update")
}

/// Run the auto-update if policy says so; never fails the calling command.
pub fn auto_update_if_needed(_cfg: &Config, _command: &str) {
    todo!("update::auto_update_if_needed")
}

pub fn print_report(_cfg: &Config, _report: &UpdateReport, _quiet: bool) {
    todo!("update::print_report")
}
