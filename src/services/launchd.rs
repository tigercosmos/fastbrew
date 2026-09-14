//! launchctl wrappers: `bootstrap`, `bootout`, `enable`, `kill`, `print`, `list`.

use std::path::Path;

use crate::error::Result;

/// `gui/<uid>` or `system`.
pub fn domain_target(sudo: bool) -> String {
    if sudo {
        "system".to_string()
    } else {
        format!("gui/{}", nix::unistd::getuid().as_raw())
    }
}

pub fn bootstrap(_domain: &str, _plist: &Path, _sudo: bool) -> Result<()> {
    todo!("services::launchd::bootstrap")
}

pub fn bootout(_domain: &str, _label: &str, _sudo: bool) -> Result<()> {
    todo!("services::launchd::bootout")
}

pub fn enable(_domain: &str, _label: &str, _sudo: bool) -> Result<()> {
    todo!("services::launchd::enable")
}

pub fn kill(_domain: &str, _label: &str, _signal: &str, _sudo: bool) -> Result<()> {
    todo!("services::launchd::kill")
}

#[derive(Debug, Clone, Default)]
pub struct PrintInfo {
    pub running: bool,
    pub pid: Option<u32>,
    pub last_exit_code: Option<i32>,
    pub path: Option<String>,
}

/// `launchctl print <domain>/<label>`; `None` when not loaded.
pub fn print(_domain: &str, _label: &str, _sudo: bool) -> Result<Option<PrintInfo>> {
    todo!("services::launchd::print")
}
