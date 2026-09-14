//! `services` command and launchd plist generation (`docs/COMPAT.md` 7).

pub mod launchd;
pub mod plist;

use crate::config::Config;
use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceStatus {
    None,
    Started,
    Scheduled,
    Stopped,
    Error(i32),
    Unknown,
    Other,
}

#[derive(Debug, Clone)]
pub struct ServiceInfo {
    pub name: String,
    pub label: String,
    pub status: ServiceStatus,
    pub user: Option<String>,
    /// Plist path in use (`~/Library/LaunchAgents/...` when loaded, else the keg plist).
    pub file: Option<std::path::PathBuf>,
    pub loaded: bool,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
}

/// All installed formulae that ship a service, with their current status.
pub fn list(_cfg: &Config) -> Result<Vec<ServiceInfo>> {
    todo!("services::list")
}

pub fn start(_cfg: &Config, _name: &str, _sudo: bool) -> Result<()> {
    todo!("services::start")
}

pub fn stop(_cfg: &Config, _name: &str, _sudo: bool) -> Result<()> {
    todo!("services::stop")
}

pub fn restart(_cfg: &Config, _name: &str, _sudo: bool) -> Result<()> {
    todo!("services::restart")
}

/// `run`: bootstrap without enabling at login.
pub fn run(_cfg: &Config, _name: &str, _sudo: bool) -> Result<()> {
    todo!("services::run")
}

pub fn kill(_cfg: &Config, _name: &str, _sudo: bool) -> Result<()> {
    todo!("services::kill")
}

pub fn info(_cfg: &Config, _name: &str) -> Result<ServiceInfo> {
    todo!("services::info")
}

/// Remove service files for uninstalled formulae.
pub fn cleanup(_cfg: &Config) -> Result<()> {
    todo!("services::cleanup")
}
