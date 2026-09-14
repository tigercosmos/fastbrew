//! launchctl wrappers: `bootstrap`, `bootout`, `enable`, `kill`, `print`, `list`.
//!
//! Port of `Library/Homebrew/services/system.rb` and the launchctl branches of
//! `services/cli.rb`. Nothing here is executed by the unit tests: the output
//! parsers are pure functions so they can be tested against captured text.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::config::Config;
use crate::error::{Error, Result};

/// `launchctl bootout` exit status meaning the domain does not support the action.
pub const DOMAIN_ACTION_NOT_SUPPORTED: i32 = 125;

/// `gui/<uid>` or `system`.
pub fn domain_target(sudo: bool) -> String {
    if sudo {
        "system".to_string()
    } else {
        format!("gui/{}", nix::unistd::getuid().as_raw())
    }
}

/// Every domain Homebrew probes for a user service (`System.candidate_domain_targets`).
pub fn candidate_domain_targets(sudo: bool) -> Vec<String> {
    if sudo {
        return vec!["system".to_string()];
    }
    let uid = nix::unistd::getuid().as_raw();
    let mut out = vec![domain_target(false)];
    for candidate in [format!("user/{uid}"), format!("gui/{uid}")] {
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// Running as root (`System.root?`).
pub fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// `/Library/LaunchDaemons` (`System.boot_path`).
pub fn boot_path() -> PathBuf {
    PathBuf::from("/Library/LaunchDaemons")
}

/// `~/Library/LaunchAgents` (`System.user_path`).
pub fn user_path(cfg: &Config) -> PathBuf {
    cfg.home.join("Library/LaunchAgents")
}

/// Where a service file belongs for this invocation (`System.path`).
pub fn dest_dir(cfg: &Config, sudo: bool) -> PathBuf {
    if sudo || is_root() {
        boot_path()
    } else {
        user_path(cfg)
    }
}

/// Current user name (`System.user`).
pub fn current_user() -> Option<String> {
    if let Ok(u) = std::env::var("USER")
        && !u.is_empty()
    {
        return Some(u);
    }
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|u| u.name)
}

/// Path to `launchctl`, or `None` on a system without launchd.
pub fn launchctl_path() -> Option<PathBuf> {
    for candidate in ["/bin/launchctl", "/usr/bin/launchctl"] {
        let p = Path::new(candidate);
        if p.exists() {
            return Some(p.to_path_buf());
        }
    }
    which("launchctl")
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn run(args: &[&str], sudo: bool) -> Result<Output> {
    let launchctl = launchctl_path().ok_or_else(|| {
        Error::user("`brew services` is supported only on macOS or Linux (with systemd)!")
    })?;
    let mut cmd = if sudo && !is_root() {
        let mut c = Command::new("/usr/bin/sudo");
        c.arg("-E").arg("--").arg(&launchctl);
        c
    } else {
        Command::new(&launchctl)
    };
    cmd.args(args);
    cmd.output()
        .map_err(|e| Error::user(format!("Failed to run {}: {e}", launchctl.display())))
}

fn run_checked(args: &[&str], sudo: bool) -> Result<()> {
    let out = run(args, sudo)?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail = stderr.trim();
    Err(Error::user(format!(
        "Failure while executing; `launchctl {}` exited with {}.{}",
        args.join(" "),
        out.status.code().unwrap_or(-1),
        if detail.is_empty() {
            String::new()
        } else {
            format!("\n{detail}")
        }
    )))
}

pub fn bootstrap(domain: &str, plist: &Path, sudo: bool) -> Result<()> {
    run_checked(&["bootstrap", domain, &plist.to_string_lossy()], sudo)
}

pub fn bootout(domain: &str, label: &str, sudo: bool) -> Result<()> {
    run_checked(&["bootout", &format!("{domain}/{label}")], sudo)
}

/// `launchctl bootout`, reporting success instead of raising (used by `stop`).
pub fn try_bootout(domain: &str, label: &str, sudo: bool) -> bool {
    run(&["bootout", &format!("{domain}/{label}")], sudo)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn enable(domain: &str, label: &str, sudo: bool) -> Result<()> {
    run_checked(&["enable", &format!("{domain}/{label}")], sudo)
}

pub fn kill(domain: &str, label: &str, signal: &str, sudo: bool) -> Result<()> {
    run_checked(&["kill", signal, &format!("{domain}/{label}")], sudo)
}

/// `launchctl stop <label>`, the fallback used when `bootout` leaves it running.
pub fn stop(label: &str, sudo: bool) -> bool {
    run(&["stop", label], sudo)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[derive(Debug, Clone, Default)]
pub struct PrintInfo {
    pub running: bool,
    pub pid: Option<u32>,
    pub last_exit_code: Option<i32>,
    pub path: Option<String>,
}

/// Which launchctl subcommand produced a status blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// `launchctl print <domain>/<label>`.
    Print,
    /// `launchctl list <label>`.
    List,
}

/// Raw status output plus the label and domain it came from.
#[derive(Debug, Clone)]
pub struct Status {
    pub output: String,
    pub kind: StatusKind,
    pub label: String,
    pub success: bool,
}

impl Status {
    pub fn info(&self) -> PrintInfo {
        parse_status(&self.output, self.kind)
    }
}

/// `launchctl print <domain>/<label>`; `None` when not loaded.
pub fn print(domain: &str, label: &str, sudo: bool) -> Result<Option<PrintInfo>> {
    let out = run(&["print", &format!("{domain}/{label}")], sudo)?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(parse_status(&text, StatusKind::Print)))
}

/// `System.launchctl_find_service`: try every domain, then `launchctl list`.
pub fn find_service(label: &str, sudo: bool) -> Option<Status> {
    let lc = launchctl_path()?;
    let _ = lc;
    for domain in candidate_domain_targets(sudo) {
        if let Ok(out) = run(&["print", &format!("{domain}/{label}")], sudo)
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
            if !text.is_empty() {
                return Some(Status {
                    output: text,
                    kind: StatusKind::Print,
                    label: label.to_string(),
                    success: true,
                });
            }
        }
    }
    let out = run(&["list", label], sudo).ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    let success = out.status.success() && !text.is_empty();
    Some(Status {
        output: text,
        kind: StatusKind::List,
        label: label.to_string(),
        success,
    })
}

/// True when any domain knows about `label` (`System.launchctl_service_running?`).
pub fn service_loaded(label: &str, sudo: bool) -> bool {
    find_service(label, sudo).is_some_and(|s| s.success)
}

/// Every label `launchctl list` reports for this user (`Cli.running`).
///
/// One process instead of a `launchctl print` per label and domain; callers
/// re-read it after any change, since it is a snapshot.
pub fn loaded_labels() -> std::collections::HashSet<String> {
    let empty = std::collections::HashSet::new();
    let Ok(output) = run(&["list"], false) else {
        return empty;
    };
    if !output.status.success() {
        return empty;
    }
    parse_list_labels(&String::from_utf8_lossy(&output.stdout))
}

/// Third column of `launchctl list` (`PID\tStatus\tLabel`), header skipped.
pub fn parse_list_labels(text: &str) -> std::collections::HashSet<String> {
    text.lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .filter(|label| *label != "Label")
        .map(str::to_string)
        .collect()
}

/// Parse a `launchctl print` or `launchctl list` status blob.
///
/// `print` uses `state = running`, `pid = N`, `last exit code = N`, `path = ...`;
/// `list` uses `"PID" = N;` and `"LastExitStatus" = N;`.
pub fn parse_status(output: &str, kind: StatusKind) -> PrintInfo {
    let mut info = PrintInfo::default();
    match kind {
        StatusKind::Print => {
            for line in output.lines() {
                let t = line.trim();
                if let Some(v) = field(t, "state = ") {
                    info.running = v.trim() == "running";
                } else if let Some(v) = field(t, "pid = ") {
                    info.pid = digits(v).and_then(|d| d.parse().ok());
                } else if let Some(v) = field(t, "last exit code = ") {
                    info.last_exit_code = digits(v).and_then(|d| d.parse().ok());
                } else if let Some(v) = field(t, "path = ") {
                    info.path = Some(v.trim().to_string());
                }
            }
            if info.pid.is_some_and(|p| p > 0) {
                info.running = true;
            }
        }
        StatusKind::List => {
            for line in output.lines() {
                let t = line.trim();
                if let Some(rest) = t.strip_prefix("\"PID\" = ") {
                    info.pid = rest.trim_end_matches(';').trim().parse().ok();
                } else if let Some(rest) = t.strip_prefix("\"LastExitStatus\" = ") {
                    info.last_exit_code = rest.trim_end_matches(';').trim().parse().ok();
                }
            }
            info.running = info.pid.is_some_and(|p| p > 0);
        }
    }
    info
}

/// Homebrew's regexes are unanchored, so match the key anywhere in the line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.find(key).map(|i| &line[i + key.len()..])
}

fn digits(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 { None } else { Some(&s[..end]) }
}

/// Read `Label` (and `UserName`) out of an installed plist.
pub fn read_plist_label(path: &Path) -> Option<String> {
    read_plist_string(path, "Label")
}

/// Read one top-level string key out of an XML (or binary) plist.
pub fn read_plist_string(path: &Path, key: &str) -> Option<String> {
    read_plist_dictionary(path)?
        .get(key)?
        .as_string()
        .map(str::to_string)
}

/// Parse an XML (or binary) plist into its top-level dictionary.
pub fn read_plist_dictionary(path: &Path) -> Option<plist::Dictionary> {
    plist::Value::from_file(path).ok()?.into_dictionary()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRINT_RUNNING: &str = r#"gui/501/homebrew.mxcl.unbound = {
	active count = 1
	path = /Users/example/Library/LaunchAgents/homebrew.mxcl.unbound.plist
	type = LaunchAgent
	state = running

	program = /opt/homebrew/opt/unbound/sbin/unbound
	arguments = {
		/opt/homebrew/opt/unbound/sbin/unbound
		-d
	}

	inherited environment = {
	}

	pid = 4242
	immediate reason = speculative
	forks = 0
	execs = 1
	initialized = 1
	trampolined = 1
	started suspended = 0
	proxies suspended = 0
	last exit code = (never exited)
}
"#;

    const PRINT_STOPPED: &str = r#"gui/501/homebrew.mxcl.redis = {
	active count = 0
	path = /Users/example/Library/LaunchAgents/homebrew.mxcl.redis.plist
	state = not running
	last exit code = 0
	runs = 1
}
"#;

    const PRINT_ERROR: &str = r#"gui/501/homebrew.mxcl.foo = {
	path = /Users/example/Library/LaunchAgents/homebrew.mxcl.foo.plist
	state = not running
	last exit code = 78
}
"#;

    const LIST_OUTPUT: &str = r#"{
	"LimitLoadToSessionType" = "Aqua";
	"Label" = "homebrew.mxcl.redis";
	"OnDemand" = false;
	"LastExitStatus" = 0;
	"PID" = 1234;
	"Program" = "/opt/homebrew/opt/redis/bin/redis-server";
};
"#;

    #[test]
    fn parses_launchctl_print() {
        let info = parse_status(PRINT_RUNNING, StatusKind::Print);
        assert!(info.running);
        assert_eq!(info.pid, Some(4242));
        // `(never exited)` has no digits, so there is no exit code.
        assert_eq!(info.last_exit_code, None);
        assert_eq!(
            info.path.as_deref(),
            Some("/Users/example/Library/LaunchAgents/homebrew.mxcl.unbound.plist")
        );

        let info = parse_status(PRINT_STOPPED, StatusKind::Print);
        assert!(!info.running);
        assert_eq!(info.pid, None);
        assert_eq!(info.last_exit_code, Some(0));

        let info = parse_status(PRINT_ERROR, StatusKind::Print);
        assert_eq!(info.last_exit_code, Some(78));
        assert!(!info.running);
    }

    #[test]
    fn parses_launchctl_list() {
        let info = parse_status(LIST_OUTPUT, StatusKind::List);
        assert_eq!(info.pid, Some(1234));
        assert_eq!(info.last_exit_code, Some(0));
        assert!(info.running);
    }

    #[test]
    fn parses_launchctl_list_labels() {
        let text = "PID\tStatus\tLabel\n\
                    -\t0\tcom.apple.SafariHistoryServiceAgent\n\
                    4242\t0\thomebrew.mxcl.unbound\n\
                    -\t78\tsh.brew.redis\n";
        let labels = parse_list_labels(text);
        assert!(labels.contains("homebrew.mxcl.unbound"));
        assert!(labels.contains("sh.brew.redis"));
        assert!(!labels.contains("Label"));
        assert_eq!(labels.len(), 3);
    }

    #[test]
    fn domain_targets() {
        let uid = nix::unistd::getuid().as_raw();
        assert_eq!(domain_target(true), "system");
        assert_eq!(domain_target(false), format!("gui/{uid}"));
        let candidates = candidate_domain_targets(false);
        assert_eq!(candidates[0], format!("gui/{uid}"));
        assert!(candidates.contains(&format!("user/{uid}")));
        assert_eq!(candidate_domain_targets(true), vec!["system".to_string()]);
    }
}
